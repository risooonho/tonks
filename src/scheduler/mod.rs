use bit_set::BitSet;
use bumpalo::Bump;
use crossbeam::{Receiver, Sender};
use rayon::prelude::*;
use smallvec::{smallvec, SmallVec};
use std::collections::VecDeque;
use thread_local::ThreadLocal;

mod builder;

use crate::{resources::RESOURCE_ID_MAPPINGS, RawSystem, ResourceId, Resources, SystemId};
pub use builder::SchedulerBuilder;

/// Context of a running system, used for internal purposes.
#[derive(Clone)]
pub struct Context {
    /// The ID of this system.
    pub(crate) id: SystemId,
    /// Sender for communicating with the scheduler.
    pub(crate) sender: Sender<TaskMessage>,
}

/// ID of a stage, allocated consecutively for use as indices into vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct StageId(usize);

/// A stage in the completion of a dispatch. Each stage
/// contains systems which can be executed in parallel.
type Stage = SmallVec<[SystemId; 6]>;

type ResourceVec = SmallVec<[ResourceId; 8]>;

/// A raw pointer to some `T`.
///
/// # Safety
/// This type implements `Send` and `Sync`, but it is
/// up to the user to ensure that:
/// * If the pointer is dereferenced, the value has not been dropped.
/// * No data races occur.
struct SharedRawPtr<T: ?Sized>(*const T);

unsafe impl<T: ?Sized + Send> Send for SharedRawPtr<T> {}
unsafe impl<T: ?Sized + Sync> Sync for SharedRawPtr<T> {}

type DynSystem = (dyn RawSystem + 'static);

/// A mutable raw pointer to some `T`.
///
/// # Safety
/// This type implements `Send` and `Sync`, but it is
/// up to the user to ensure that:
/// * If the pointer is dereferenced, the value has not been dropped.
/// * No data races occur.
struct SharedMutRawPtr<T: ?Sized>(*mut T);

unsafe impl<T: ?Sized + Send> Send for SharedMutRawPtr<T> {}
unsafe impl<T: ?Sized + Sync> Sync for SharedMutRawPtr<T> {}

/// A message sent from a running task to the scheduler.
#[allow(dead_code)]
pub(crate) enum TaskMessage {
    /// Indicates that the system with the given ID
    /// completed.
    ///
    /// Note that this is not sent for ordinary systems in
    /// a stage, since the overhead of sending so many
    /// messages would be too great. Instead, `StageComplete`
    /// is sent to indicate that all systems in a stage completed
    /// at once.
    ///
    /// This is only used for oneshot systems and event handlers.
    SystemComplete(SystemId),
    /// Indicates that all systems in a stage have completed.
    StageComplete(StageId),
}

/// A task to run. This can either be a stage (mutliple systems run in parallel),
/// a oneshot system, or an event handling pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
enum Task {
    Stage(StageId),
    Oneshot(SystemId),
    // TODO: event pipeline
}

/// The `tonks` scheduler. This is similar to `shred::Dispatcher`
/// but has more features.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct Scheduler {
    /// Resources held by this scheduler.
    #[derivative(Debug = "ignore")]
    resources: Resources,

    /// Queue of tasks to run during the current dispatch.
    ///
    /// Each dispatch, this queue is reset to `starting_queue`.
    task_queue: VecDeque<Task>,
    /// The starting task queue for any dispatch.
    starting_queue: VecDeque<Task>,

    /// Bit set representing write resources which are currently held.
    ///
    /// This set is indexed by the `ResourceId`.
    writes_held: BitSet,
    /// Vector of reference counts representing the number of tasks currently
    /// holding a shared reference to a resource.
    ///
    /// This vector is indexed by the `ResourceId`.
    reads_held: Vec<u8>,

    /// Thread-local bump allocator used to allocate events.
    ///
    /// TODO: implement a lock-free bump arena instead.
    #[derivative(Debug = "ignore")]
    bump: ThreadLocal<Bump>,

    /// Number of currently running systems.
    runnning_systems_count: usize,
    /// Bit set containing bits set for systems which are currently running.
    ///
    /// This is indexed by the `SystemId`.
    running_systems: BitSet,

    /// Vector of systems which can be executed. This includes oneshottable
    /// systems as well.
    ///
    /// This vector is indexed by the `SystemId`.
    #[derivative(Debug = "ignore")]
    systems: Vec<Box<DynSystem>>,
    /// Vector containing the systems for each stage.
    stages: Vec<Stage>,

    /// Vector containing the reads required for each system.
    ///
    /// This vector is indexed by the `SystemId`.
    system_reads: Vec<ResourceVec>,
    /// Vector containing the writes required for each system.
    ///
    /// This vector is indexed by the `SystemId`.
    system_writes: Vec<ResourceVec>,
    /// Vector containing the reads required for each stage.
    ///
    /// This vector is indexed by the `StageId`.
    stage_reads: Vec<ResourceVec>,
    /// Vector containing the writes required for each stage.
    ///
    /// This vector is indexed by the `StageId`.
    stage_writes: Vec<ResourceVec>,

    /// Receiving end of the channel used to communicate with running systems.
    #[derivative(Debug = "ignore")]
    receiver: Receiver<TaskMessage>,
    /// Sending end of the above channel. This can be cloned and sent to systems.
    #[derivative(Debug = "ignore")]
    sender: Sender<TaskMessage>,
}

impl Scheduler {
    /// Creates a new `Scheduler` with the given stages.
    ///
    /// `deps` is a vector indexed by the system ID containing
    /// resources for each system.
    ///
    /// # Contract
    /// The stages are assumed to have been assembled correctly:
    /// no two systems in a stage may conflict with each other.
    pub fn new(
        stages: Vec<Vec<Box<DynSystem>>>,
        read_deps: Vec<Vec<ResourceId>>,
        write_deps: Vec<Vec<ResourceId>>,
        resources: Resources,
    ) -> Self {
        // Detect resources used by systems and create those vectors.
        // Also collect systems into uniform vector.
        let mut system_reads: Vec<ResourceVec> = vec![];
        let mut system_writes: Vec<ResourceVec> = vec![];
        let mut stage_reads: Vec<ResourceVec> = vec![];
        let mut stage_writes: Vec<ResourceVec> = vec![];
        let mut systems = vec![];
        let mut stage_systems = vec![];

        let mut counter = 0;
        for stage in stages {
            let mut stage_read = vec![];
            let mut stage_write = vec![];
            let mut systems_in_stage = smallvec![];

            for system in stage {
                system_reads.push(read_deps[counter].iter().copied().collect());
                system_writes.push(write_deps[counter].iter().copied().collect());
                stage_read.extend(system_reads[counter].clone());
                stage_write.extend(system_writes[counter].clone());
                systems.push(system.into());
                systems_in_stage.push(SystemId(counter));
                counter += 1;
            }

            stage_reads.push(stage_read.into_iter().collect());
            stage_writes.push(stage_write.into_iter().collect());
            stage_systems.push(systems_in_stage);
        }

        // We use a bounded channel because the only overhead
        // is typically on the sender's side—the receiver, the scheduler, should
        // plow through messages. This may be changed in the future.
        let (sender, receiver) = crossbeam::bounded(8);

        let bump = ThreadLocal::new();

        let starting_queue = Self::create_task_queue(&stage_systems);

        Self {
            resources,

            starting_queue,
            task_queue: VecDeque::new(), // Replaced in `execute()`

            writes_held: BitSet::new(),
            reads_held: vec![0; RESOURCE_ID_MAPPINGS.lock().len()],

            runnning_systems_count: 0,
            running_systems: BitSet::with_capacity(systems.len()),

            systems,
            stages: stage_systems,

            system_reads,
            system_writes,
            stage_reads,
            stage_writes,

            bump,

            sender,
            receiver,
        }
    }

    fn create_task_queue(stages: &[Stage]) -> VecDeque<Task> {
        stages
            .iter()
            .enumerate()
            .map(|(id, _)| Task::Stage(StageId(id)))
            .collect()
    }

    /// Executes all systems and handles events.
    pub fn execute(&mut self) {
        // Reset the task queue to the starting queue.
        assert!(self.task_queue.is_empty());
        self.task_queue.extend(self.starting_queue.iter().copied());

        // While there are remaining tasks, dispatch them.
        // When we encounter a task which can't be run because
        // of conflicting dependencies, we wait for tasks to
        // complete by listening on the channel.
        while let Some(task) = self.task_queue.pop_front() {
            // Attempt to run task.
            self.run_task(task);
        }

        // Wait for remaining systems to complete.
        while self.runnning_systems_count > 0 {
            let num = self.wait_for_completion();
            self.runnning_systems_count -= num;

            // Run any handlers/oneshots scheduled by these systems
            while let Some(task) = self.task_queue.pop_front() {
                self.run_task(task);
            }
        }
    }

    fn run_task(&mut self, task: Task) {
        let reads = reads_for_task(&self.stage_reads, &self.system_reads, &task);
        let writes = writes_for_task(&self.stage_writes, &self.system_writes, &task);

        match try_obtain_resources(reads, writes, &mut self.reads_held, &mut self.writes_held) {
            Ok(()) => {
                // Run task and proceed.
                let systems = self.dispatch_task(task);
                self.runnning_systems_count += systems;
            }
            Err(()) => {
                // Execution is blocked: wait for tasks to finish.
                // Re-push the task we attempted to run to the queue.
                // TODO: optimize this
                self.task_queue.push_front(task);
                let num = self.wait_for_completion();
                self.runnning_systems_count -= num;
            }
        }
    }

    /// Waits for messages from running systems and handles them.
    ///
    /// At any point, returns with the number of systems which have completed.
    fn wait_for_completion(&mut self) -> usize {
        // Unwrap is allowed because the channel never becomes disconnected
        // (`Scheduler` holds a `Sender` handle for it).
        // This will never block indefinitely because there are always
        // systems running when this is invoked.
        let msg = self.receiver.recv().unwrap();

        match msg {
            // TODO: events
            TaskMessage::SystemComplete(id) => {
                self.release_resources_for_system(id);
                self.running_systems.remove(id.0);
                1
            }
            TaskMessage::StageComplete(id) => {
                self.release_resources_for_stage(id);
                let running_systems = &mut self.running_systems;
                self.stages[id.0].iter().for_each(|id| {
                    running_systems.remove(id.0);
                });
                self.stages[id.0].len()
            }
        }
    }

    fn release_resources_for_system(&mut self, id: SystemId) {
        let reads = &self.system_reads[id.0];
        let writes = &self.system_writes[id.0];

        for read in reads {
            self.reads_held[read.0] -= 1;
        }

        for write in writes {
            self.writes_held.remove(write.0);
        }
    }

    fn release_resources_for_stage(&mut self, id: StageId) {
        for read in &self.stage_reads[id.0] {
            self.reads_held[read.0] -= 1;
        }

        for write in &self.stage_writes[id.0] {
            self.writes_held.remove(write.0);
        }
    }

    /// Dispatches a task, returning the number of systems spawned.
    fn dispatch_task(&mut self, task: Task) -> usize {
        match task {
            Task::Stage(id) => {
                let running_systems = &mut self.running_systems;
                self.stages[id.0].iter().for_each(|id| {
                    running_systems.insert(id.0);
                });
                self.dispatch_stage(id);
                self.stages[id.0].len()
            }
            Task::Oneshot(id) => {
                self.running_systems.insert(id.0);
                self.dispatch_system(id);
                1
            }
        }
    }

    fn dispatch_stage(&mut self, id: StageId) {
        // Rather than spawning each system independently, we optimize
        // this by running them in batch. This reduces synchronization overhead
        // with the scheduler using channels.

        // Safety of these raw pointers: they remain valid as long as the scheduler
        // is still in `execute()`, and `execute()` will not return until all systems
        // have completed.
        let stage = SharedRawPtr(&self.stages[id.0] as *const Stage);
        let resources = SharedRawPtr(&self.resources as *const Resources);

        let systems = SharedMutRawPtr(&mut self.systems as *mut Vec<Box<DynSystem>>);

        let sender = self.sender.clone();

        rayon::spawn(move || {
            unsafe {
                (&*stage.0)
                    .par_iter()
                    .map(|sys_id| &mut (&mut *systems.0)[sys_id.0])
                    .for_each(|sys| sys.execute_raw(&*(resources.0)));
            }

            // TODO: events, oneshot
            sender.send(TaskMessage::StageComplete(id)).unwrap();
        });
    }

    fn dispatch_system(&mut self, id: SystemId) {
        let resources = SharedRawPtr(&self.resources as *const Resources);
        let system = SharedMutRawPtr(&mut *self.systems[id.0] as *mut DynSystem);

        let sender = self.sender.clone();
        rayon::spawn(move || {
            unsafe {
                // Safety: the world is not dropped while the system
                // executes, since `execute` will not return until
                // all systems have completed.
                (&mut *system.0).execute_raw(&*(resources.0));
            }

            // TODO: events
            sender.send(TaskMessage::SystemComplete(id)).unwrap();
        });
    }
}

/// Attempts to acquire resources for a task, returning `Err` if
/// there was a conflict and `Ok` if successful.
fn try_obtain_resources(
    reads: &ResourceVec,
    writes: &ResourceVec,
    reads_held: &mut [u8],
    writes_held: &mut BitSet,
) -> Result<(), ()> {
    // First, go through resources and confirm that there are no conflicting
    // accessors.
    // Since both read and write dependencies will only conflict with another resource
    // access when there is another write access, we can interpret them in the same way.
    for resource in reads.iter().chain(writes) {
        if writes_held.contains(resource.0) {
            return Err(()); // Conflict
        }
    }
    // Write resources will also conflict with existing read ones.
    for resource in writes {
        if reads_held[resource.0] > 0 {
            return Err(()); // Conflict
        }
    }

    // Now obtain resources by updating internal structures.
    for read in reads {
        reads_held[read.0] += 1;
    }

    for write in writes {
        writes_held.insert(write.0);
    }

    Ok(())
}

fn reads_for_task<'a>(
    stage_reads: &'a [ResourceVec],
    system_reads: &'a [ResourceVec],
    task: &Task,
) -> &'a ResourceVec {
    match task {
        Task::Stage(id) => &stage_reads[id.0],
        Task::Oneshot(id) => &system_reads[id.0],
    }
}

fn writes_for_task<'a>(
    stage_writes: &'a [ResourceVec],
    system_writes: &'a [ResourceVec],
    task: &Task,
) -> &'a ResourceVec {
    match task {
        Task::Stage(id) => &stage_writes[id.0],
        Task::Oneshot(id) => &system_writes[id.0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_scheduler_traits() {
        static_assertions::assert_impl_all!(Scheduler: Send, Sync);
    }
}
