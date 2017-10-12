use std::net::{SocketAddr};
use std::collections::HashMap;

use futures::{Future, Stream};
use tokio_core::reactor::Handle;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_io::AsyncRead;
use capnp_rpc::{RpcSystem, twoparty, rpc_twoparty_capnp};

use errors::Result;
use common::id::{SessionId, WorkerId, DataObjectId, TaskId, ClientId, SId};
use common::rpc::new_rpc_system;
use server::graph::{Graph, WorkerRef, DataObjectRef, TaskRef, SessionRef,
                    ClientRef, DataObjectState, DataObjectType, TaskState, TaskInput};
use server::rpc::ServerBootstrapImpl;
use server::scheduler::{Scheduler, RandomScheduler, UpdatedIn, UpdatedOut};
use common::convert::ToCapnp;
use common::wrapped::WrappedRcRefCell;
use common::resources::Resources;
use common::{Additional, ConsistencyCheck};
use common::events::Event;

pub struct State {
    // Contained objects
    pub(super) graph: Graph,

    /// If true, next "turn" the scheduler is executed
    need_scheduling: bool,

    /// Listening port and address.
    listen_address: SocketAddr,

    /// Tokio core handle.
    handle: Handle,

    stop_server: bool,

    updates: UpdatedIn,

    scheduler: RandomScheduler,

    self_ref: Option<StateRef>,
}

impl State {

    /// Add new worker, register it in the graph
    pub fn add_worker(&mut self,
                      address: SocketAddr,
                      control: Option<::worker_capnp::worker_control::Client>,
                      resources: Resources) -> Result<WorkerRef> {
        debug!("New worker {}", address);
        if self.graph.workers.contains_key(&address) {
            bail!("State already contains worker {}", address);
        }
        let w = WorkerRef::new(address, control, resources);
        self.graph.workers.insert(w.get_id(), w.clone());
        Ok(w)
    }

    /// Remove the worker from the graph, forcefully unassigning all tasks and objects.
    /// TODO: better specs and context of worker removal
    pub fn remove_worker(&mut self, worker: &WorkerRef) -> Result<()> {
        unimplemented!() /*
            pub fn delete(self, graph: &mut Graph) {
        debug!("Deleting worker {}", self.get_id());
        // remove from objects
        for o in self.get_mut().assigned_objects.iter() {
            assert!(o.get_mut().assigned.remove(&self));
        }
        for o in self.get_mut().located_objects.iter() {
            assert!(o.get_mut().located.remove(&self));
        }
        // remove from tasks
        for t in self.get_mut().assigned_tasks.iter() {
            t.get_mut().assigned = None;
        }
        for t in self.get_mut().scheduled_tasks.iter() {
            t.get_mut().scheduled = None;
        }
        // remove from graph
        graph.workers.remove(&self.get().id).unwrap();
        // assert that we hold the last reference, then drop it
        assert_eq!(self.get_num_refs(), 1);
        */

    }

    /// Put the worker into a failed state, unassigning all tasks and objects.
    /// Needs a lot of cleanup and recovery to avoid panic. Now just panics :-)
    pub fn fail_worker(&mut self, worker: &mut WorkerRef, cause: String) -> Result<()> {
        debug!("Failing worker {} with cause {:?}", worker.get_id(), cause);
        assert!(worker.get_mut().error.is_none());
        worker.get_mut().error = Some(cause.clone());
        // TODO: Cleanup and recovery if possible
        panic!("Worker {} error: {:?}", worker.get_id(), cause);
    }

    /// Add new client, register it in the graph
    pub fn add_client(&mut self, address: SocketAddr) -> Result<ClientRef> {
        debug!("New client {}", address);
        if self.graph.clients.contains_key(&address) {
            bail!("State already contains client {}", address);
        }
        let c = ClientRef::new(address);
        self.graph.clients.insert(c.get().id, c.clone());
        Ok(c)
    }

    /// Remove Client and its (owned) sessions. Called on client disconnect,
    /// so assume the client is inaccesible.
    pub fn remove_client(&mut self, client: &ClientRef)  -> Result<()> {
        // remove owned sessions
        let sessions = client.get().sessions.iter().map(|x| x.clone()).collect::<Vec<_>>();
        for s in sessions { self.remove_session(&s)?; }
        // remove from graph
        self.graph.clients.remove(&client.get_id()).unwrap();
        Ok(())
    }

    /// Create a new session fr a client, register it in the graph.
    pub fn add_session(&mut self, client: &ClientRef) -> Result<SessionRef> {
        Ok(SessionRef::new(self.graph.new_session_id(), client))
    }

    /// Helper for .remove_session() and .fail_session(). Remove all session tasks,
    /// objects and cancel all finish hooks.
    fn clear_session(&mut self, s: &SessionRef) -> Result<()> {
        let tasks = s.get_mut().tasks.iter().map(|x| x.clone()).collect::<Vec<_>>();
        for t in tasks { self.remove_task(&t)?; }
        let objects = s.get_mut().objects.iter().map(|x| x.clone()).collect::<Vec<_>>();
        for o in objects { self.remove_object(&o)?; }
        // Remove all finish hooks
        s.get_mut().finish_hooks.clear();
        Ok(())
    }

    /// Remove a session and all the tasks and objects, both from the graph and from the workers,
    /// cancel all the finish hooks.
    pub fn remove_session(&mut self, session: &SessionRef) -> Result<()> {
        debug!("Removing session {} of client {}", session.get_id(),
               session.get().client.get_id());
        // remove children objects
        self.clear_session(session)?;
        // remove from graph
        self.graph.sessions.remove(&session.get_id()).unwrap();
        // remove from owner
        let s = session.get_mut();
        assert!(s.client.get_mut().sessions.remove(&session));
        Ok(())
    }

    /// Put the session into a failed state, removing all tasks and objects,
    /// cancelling all finish_hooks.
    pub fn fail_session(&mut self, session: &SessionRef, cause: Event) -> Result<()> {
        debug!("Failing session {} of client {} with cause {:?}", session.get_id(),
               session.get().client.get_id(), cause);
        assert!(session.get_mut().error.is_none());
        session.get_mut().error = Some(cause);
        // Remove all tasks + objects (with their finish hooks)
        self.clear_session(session)
    }

    /// Add a new object, register it in the graph and the session.
    pub fn add_object(&mut self,
               session: &SessionRef,
               id: DataObjectId,
               object_type: DataObjectType,
               client_keep: bool,
               label: String,
               data: Option<Vec<u8>>,
               additional: Additional) -> Result<DataObjectRef> {
        if self.graph.objects.contains_key(&id) {
            bail!("State already contains object with id {}", id);
        }
        let oref = DataObjectRef::new(session, id, object_type, client_keep,
                                   label, data, additional);
        // add to graph
        self.graph.objects.insert(oref.get_id(), oref.clone());
        // add to updated objects
        self.updates.new_objects.insert(oref.clone());
        oref.check_consistency_opt().unwrap(); // non-recoverable
        Ok(oref)
    }

    /// Remove the object from the graph and workers (with RPC calls).
    /// Fails with no change in the graph if there are any tasks linked to the object.
    pub fn remove_object(&mut self, oref: &DataObjectRef) -> Result<()> {
        oref.check_consistency_opt().unwrap(); // non-recoverable
        // unassign the object
        let mut ws = oref.get().assigned.clone();
        for w in ws {
            self.unassign_object(oref, &w);
        }
        // unlink from owner, consistency checks
        oref.unlink();
        // remove from graph
        self.graph.objects.remove(&oref.get_id()).unwrap();
        Ok(())
    }

    /// Add the task to the graph, checking consistency with adjacent objects.
    /// All the inputs+outputs must already be present.
    pub fn add_task(&mut self,
        session: &SessionRef,
        id: TaskId,
        inputs: Vec<TaskInput>,
        outputs: Vec<DataObjectRef>,
        task_type: String,
        task_config: Vec<u8>,
        additional: Additional,) -> Result<TaskRef> {
        if self.graph.tasks.contains_key(&id) {
            bail!("Task {} already in the graph", id);
        }
        let tref = TaskRef::new(session, id, inputs, outputs, task_type, task_config, additional)?;
        // add to graph
        self.graph.tasks.insert(tref.get_id(), tref.clone());
        // add to scheduler updates
        self.updates.new_tasks.insert(tref.clone());
        tref.check_consistency_opt().unwrap(); // non-recoverable
        Ok(tref)
    }

    /// Remove task from the graph, from the workers and unlink from adjacent objects.
    /// WARNING: May leave objects without producers. You should check for them after removing all
    /// the tasks and objects in bulk.
    pub fn remove_task(&mut self, tref: &TaskRef) -> Result<()> {
        tref.check_consistency_opt().unwrap(); // non-recoverable
        // unassign from worker
        if tref.get().assigned.is_some() {
            self.unassign_task(tref);
        }
        // Unlink from parent and objects.
        tref.unlink();
        // Remove from graph
        self.graph.tasks.remove(&tref.get_id()).unwrap();
        Ok(())
    }

    pub fn worker_by_id(&self, id: WorkerId) -> Result<WorkerRef> {
        match self.graph.workers.get(&id) {
            Some(w) => Ok(w.clone()),
            None => Err(format!("Worker {:?} not found", id))?,
        }
    }

    pub fn client_by_id(&self, id: ClientId) -> Result<ClientRef> {
        match self.graph.clients.get(&id) {
            Some(c) => Ok(c.clone()),
            None => Err(format!("Client {:?} not found", id))?,
        }
    }

    pub fn session_by_id(&self, id: SessionId) -> Result<SessionRef> {
        match self.graph.sessions.get(&id) {
            Some(s) => Ok(s.clone()),
            None => Err(format!("Session {:?} not found", id))?,
        }
    }

    pub fn object_by_id(&self, id: DataObjectId) -> Result<DataObjectRef> {
        match self.graph.objects.get(&id) {
            Some(o) => Ok(o.clone()),
            None => Err(format!("Object {:?} not found", id))?,
        }
    }

    pub fn task_by_id(&self, id: TaskId) -> Result<TaskRef> {
        match self.graph.tasks.get(&id) {
            Some(t) => Ok(t.clone()),
            None => Err(format!("Task {:?} not found", id))?,
        }
    }

    /// Verify submit integrity: all objects have either data or producers, acyclicity.
    pub fn verify_submit(&mut self, tasks: &[TaskRef], objects: &[DataObjectRef]) -> Result<()> {
        // TODO: Check acyclicity
        // Every object must have data or a single producer
        for oref in objects.iter() {
            let o = oref.get();
            if o.producer.is_some() && o.data.is_some() {
                bail!("Object {} submitted with both producer task {} and data of size {}",
                    o.id, o.producer.as_ref().unwrap().get_id(),
                    o.data.as_ref().unwrap().len());
            }
            if o.producer.is_none() && o.data.is_none() {
                bail!("Object {} submitted with neither producer nor data.", o.id);
            }
        }
        // Verify every submitted object
        for oref in objects.iter() {
            oref.check_consistency()?;
        }
        // Verify every submitted task
        for tref in tasks.iter() {
            tref.check_consistency()?;
        }

        self.check_consistency_opt().unwrap(); // non-recoverable
        Ok(())
    }

    /// Assign a `Finished` object to a worker and send the object metadata.
    /// Panics if the object is already assigned on the worker or not Finished.
    pub fn assign_object(&mut self, object: &DataObjectRef, wref: &WorkerRef) {
        assert_eq!(object.get().state, DataObjectState::Finished);
        assert!(!object.get().assigned.contains(wref));
        object.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable
        let empty_worker_id = ::common::id::empty_worker_id();

        // Create request
        let mut req = wref.get().control.as_ref().unwrap().add_nodes_request();
        {
            let mut new_objects = req.get().init_new_objects(1);
            let mut co = &mut new_objects.borrow().get(0);
            let o = object.get();
            o.to_worker_capnp(&mut co);
            let placement = o.located.iter().next()
                .map(|w| w.get().id().clone())
                .unwrap_or_else(|| {
                    // If there is no placement, then server is the source of datobject
                    assert!(o.data.is_some());
                    empty_worker_id.clone()
                });
            placement.to_capnp(&mut co.borrow().get_placement().unwrap());
            co.set_assigned(true);
        }

        self.handle.spawn(req
            .send().promise
            .map(|_| ())
            .map_err(|e| panic!("Send failed {:?}", e)));

        object.get_mut().assigned.insert(wref.clone());
        wref.get_mut().assigned_objects.insert(object.clone());
        object.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable
    }

    /// Unassign an object from a worker and send the unassign call.
    /// Panics if the object is not assigned on the worker.
    pub fn unassign_object(&mut self, object: &DataObjectRef, wref: &WorkerRef) {
        assert!(object.get().assigned.contains(wref));
        object.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable

        // Create request
        let mut req = wref.get().control.as_ref().unwrap().unassign_objects_request();
        {
            let mut objects = req.get().init_objects(1);
            let mut co = &mut objects.borrow().get(0);
            object.get_id().to_capnp(co);
        }

        self.handle.spawn(req
            .send().promise
            .map(|_| ())
            .map_err(|e| panic!("Send failed {:?}", e)));

        object.get_mut().assigned.remove(wref);
        wref.get_mut().assigned_objects.remove(object);

        object.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable
    }

    /// Assign and send the task to the worker it is scheduled for.
    /// Panics when the task is not scheduled or not ready.
    /// Assigns output objects to the worker, input objects are not assigned.
    pub fn assign_task(&mut self, task: &TaskRef) {
        task.check_consistency_opt().unwrap(); // non-recoverable

        let mut t = task.get_mut();
        assert!(t.scheduled.is_some());
        assert!(t.assigned.is_none());

        // Collect input objects: pairs (object, worker_id) where worker_id is placement of object
        let mut objects: Vec<(DataObjectRef, WorkerId)> = Vec::new();

        let wref = t.scheduled.as_ref().unwrap().clone();
        t.assigned = Some(wref.clone());
        let worker_id = wref.get_id();
        let empty_worker_id = ::common::id::empty_worker_id();
        debug!("Assiging task id={} to worker={}", t.id, worker_id);

        for input in t.inputs.iter() {
            let mut o = input.object.get_mut();
            if !o.assigned.contains(&wref) {

                // Just take first placement
                let placement = o.located.iter().next()
                    .map(|w| w.get().id().clone())
                    .unwrap_or_else(|| {
                        // If there is no placement, then server is the source of datobject
                        assert!(o.data.is_some());
                        empty_worker_id.clone()
                    });
                objects.push((input.object.clone(), placement));
            }
        }

        for output in t.outputs.iter() {
            objects.push((output.clone(), worker_id.clone()));
            output.get_mut().assigned.insert(wref.clone());
            wref.get_mut().assigned_objects.insert(output.clone());
        }

        // Create request
        let mut req = wref.get().control.as_ref().unwrap().add_nodes_request();

        // Serialize objects
        {
            let mut new_objects = req.get()
                .init_new_objects(objects.len() as u32);
            for (i, &(ref object, placement)) in objects.iter().enumerate() {
                let mut co = &mut new_objects.borrow().get(i as u32);
                placement.to_capnp(&mut co.borrow().get_placement().unwrap());
                let obj = object.get();
                obj.to_worker_capnp(&mut co);
                // only assign output tasks
                co.set_assigned(i >= t.inputs.len());
            }
        }

        // Serialize tasks
        {
            let mut new_tasks = req.get().init_new_tasks(1);
            t.to_worker_capnp(&mut new_tasks.get(0));
        }

        self.handle.spawn(req
            .send().promise
            .map(|_| ())
            .map_err(|e| panic!("Send failed {:?}", e)));

        wref.get_mut().assigned_tasks.insert(task.clone());
        wref.get_mut().scheduled_ready_tasks.remove(task);
        t.assigned = Some(wref);
        t.state = TaskState::Assigned;

        for oref in t.outputs.iter() {
            oref.get_mut().assigned.insert(wref);
            wref.get_mut().assigned_objects.insert(oref.clone());
        }

        task.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable
    }

    /// Unassign task from the worker it is assigned to and send the unassign call.
    /// Panics when the task is not assigned to the given worker or scheduled there.
    pub fn unassign_task(&mut self, task: &TaskRef) {
        let wref = task.get().assigned.unwrap(); // non-recoverable
        assert!(task.get().scheduled != Some(wref));
        task.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable

        // Create request
        let mut req = wref.get().control.as_ref().unwrap().stop_tasks_request();
        {
            let mut tasks = req.get().init_tasks(1);
            let mut ct = &mut tasks.borrow().get(0);
            task.get_id().to_capnp(ct);
        }

        self.handle.spawn(req
            .send().promise
            .map(|_| ())
            .map_err(|e| panic!("Send failed {:?}", e)));

        task.get_mut().assigned = None;
        task.get_mut().state = TaskState::Ready;
        wref.get_mut().assigned_tasks.remove(task);
        self.update_task_assignment(task);

        task.check_consistency_opt().unwrap(); // non-recoverable
        wref.check_consistency_opt().unwrap(); // non-recoverable
    }

    /// Removes a keep flag from an object.
    pub fn unkeep_object(&mut self, object: &DataObjectRef) {
        object.check_consistency_opt().unwrap(); // non-recoverable
        object.get_mut().client_keep = false;
        self.update_object_assignments(object, None);
        object.check_consistency_opt().unwrap(); // non-recoverable
    }

    /// Update any assignments depending on the task state, and set to Ready on all inputs ready.
    ///
    /// * Check if all task inputs are ready, and switch state.
    /// * Check if a ready task is scheduled and queue it on the worker (`scheduled_ready`).
    /// * Check if a task is assigned and not scheduled or scheduled elsewhere,
    ///   then unassign and possibly enqueue as a ready task on scheduled worker.
    /// * Check if a task is finished, then unschedule and cleanup.
    /// * Failed task is an error here.
    pub fn update_task_assignment(&mut self, tref: &TaskRef) {
        assert!(tref.get().state != TaskState::Failed);

        if tref.get().state == TaskState::NotAssigned && tref.get().waiting_for.is_empty() {
            tref.get().state = TaskState::Ready;
            self.updates.tasks.insert(tref.clone());
        }

        if tref.get().state == TaskState::Ready {
            if let Some(ref wref) = tref.get().scheduled {
                wref.get_mut().scheduled_ready_tasks.insert(tref.clone());
            }
        }

        if tref.get().state == TaskState::Assigned || tref.get().state == TaskState::Running {
            if tref.get().assigned != tref.get().scheduled {
                if let Some(ref wref) = tref.get().assigned {
                    // Unassign the task if assigned
                    self.unassign_task(tref);
                    // The state was assigned or running, now is ready
                    assert_eq!(tref.get().state, TaskState::Ready);
                }
                if let Some(ref wref) = tref.get().scheduled {
                    if tref.get().state == TaskState::Ready {
                        // If reported as updated by mistake, the task may be already in the set
                        wref.get_mut().scheduled_ready_tasks.insert(tref.clone());
                    }
                }
            }
        }

        if tref.get().state == TaskState::Finished {
            assert!(tref.get().assigned.is_none());
            tref.get_mut().scheduled = None;
        }

        tref.check_consistency_opt().unwrap(); // unrecoverable
    }

    /// Update finished object assignment to match the schedule on the given worker (optional) and
    /// needed-ness. NOP for Unfinished and Removed objects.
    ///
    /// If worker is given, updates the assignment on the worker to match the
    /// scheduling there. Object is unassigned only if located elsewhere or not needed.
    ///
    /// Then, if the object is not scheduled and not needed, it is unassigned and set to Removed.
    /// If the object is scheduled but located on more workers than scheduled on (this can happen
    /// e.g. when scheduled after the needed object was located but not scheduled), the located
    /// list is pruned to only match the scheduled list (possibly plus one remaining worker if no
    /// scheduled workers have it located).
    pub fn update_object_assignments(&mut self, oref: &DataObjectRef, worker: Option<&WorkerRef>) {
        match oref.get().state {
            DataObjectState::Unfinished => (),
            DataObjectState::Removed => (),
            DataObjectState::Finished => {
                if let Some(ref wref) = worker {
                    if wref.get().scheduled_objects.contains(oref) {
                        if !wref.get().assigned_objects.contains(oref) &&
                            oref.get().state == DataObjectState::Finished {
                            self.assign_object(oref, wref);
                        }
                    } else {
                        if wref.get().assigned_objects.contains(oref) &&
                            (!oref.is_needed() || oref.get().located.len() > 2 ||
                                !oref.get().located.contains(wref)) {
                            self.unassign_object(oref, wref);
                        }
                    }
                }
                if oref.get().scheduled.is_empty() {
                    if !oref.is_needed() {
                        for wa in oref.get().assigned.clone() {
                            self.unassign_object(oref, &wa);
                        }
                        oref.get_mut().state = DataObjectState::Removed;
                    }
                } else {
                    if oref.get().located.len() > oref.get().scheduled.len() {
                        for wa in oref.get().located.clone() {
                            if !oref.get().scheduled.contains(&wa)
                                && oref.get().located.len() >= 2 {
                                self.unassign_object(oref, &wa);
                            }
                        }
                    }
                }
            },
        }
        oref.check_consistency_opt().unwrap(); // unrecoverable
    }

    /// Process state updates from one Worker.
    pub fn updates_from_worker(&mut self,
                              worker: &WorkerRef,
                              obj_updates: &[(DataObjectRef, DataObjectState, usize, Additional)],
                              task_updates: &[(TaskRef, TaskState, Additional)]) {
        debug!("Update states for {:?}, objs: {}, tasks: {}", worker,
               obj_updates.len(), task_updates.len());
        for &(ref tref, state, additional) in task_updates {
            // inform the scheduler
            self.updates.tasks.insert(tref.clone());
            // set the state and possibly propagate
            match state {
                TaskState::Finished => {
                    tref.get_mut().state = state;
                    tref.get_mut().additional = additional;
                    self.update_task_assignment(tref);
                    tref.get_mut().trigger_finish_hooks();
                },
                TaskState::Running => {
                    assert!(tref.get().state == TaskState::Assigned);
                    tref.get_mut().state = state;
                    tref.get_mut().additional = additional;
                },
                TaskState::Failed => {
                    debug!("Task {:?} failed on {:?} with additional {:?}", *tref.get(), worker,
                           additional);
                    tref.get_mut().state = state;
                    tref.get_mut().additional = additional;
                    // TODO: Meaningful message to user
                    self.fail_session(&tref.get().session, unimplemented!());
                }
                _  => panic!("Invalid worker {:?} task {:?} state update to {:?}", worker,
                             *tref.get(), state)
            }
        }

        for &(ref oref, state, size, additional) in obj_updates {
            // Inform the scheduler
            self.updates.objects.entry(oref.clone()).or_insert(Default::default())
                .insert(worker.clone());
            match state {
                DataObjectState::Finished => {
                    oref.get_mut().located.insert(worker.clone());
                    worker.get_mut().located_objects.insert(oref.clone());
                    match oref.get().state {
                        DataObjectState::Unfinished => {
                            let mut o = oref.get_mut();
                            // first completion
                            o.state = state;
                            o.size = Some(size);
                            o.additional = additional;
                            o.trigger_finish_hooks();
                            for cref in o.consumers.iter() {
                                assert_eq!(cref.get().state, TaskState::NotAssigned);
                                cref.get_mut().waiting_for.remove(oref);
                                self.update_task_assignment(cref);
                            }
                            self.update_object_assignments(oref, Some(worker));
                        },
                        DataObjectState::Finished => {
                            // cloning to some other worker done
                            self.update_object_assignments(oref, Some(worker));
                        },
                        _ => {
                            panic!("worker {:?} set object {:?} state to {:?}", worker,
                                   *oref.get(), state);
                        }
                    }
                },
                _ => {
                    panic!("worker {:?} set object {:?} state to {:?}", worker, *oref.get(),
                        state);
                }
            }
        }
    }

    /// For all workers, if the worker is not overbooked and has ready messages, distribute
    /// more scheduled ready tasks to workers.
    pub fn distribute_tasks(&mut self) {
        for wref in self.graph.workers.values() {
            let mut w = wref.get_mut();
            // TODO: Customize the overbook limit
            while w.assigned_tasks.len() < 128 && !w.scheduled_ready_tasks.is_empty() {
                // TODO: Prioritize older members of w.scheduled_ready_tasks (order-preserving set)
                let tref = w.scheduled_ready_tasks.iter().next().unwrap().clone();
                w.scheduled_ready_tasks.remove(&tref);
                assert!(tref.get().scheduled == Some(wref.clone()));
                self.assign_task(&tref);
            }
        }
    }

    /// Run the scheduler and do any immediate updates the assignments.
    pub fn run_scheduler(&mut self) {
        debug!("Running scheduler");

        // Run scheduler and reset updated objects.
        let changed = self.scheduler.schedule(&mut self.graph, &self.updates);
        self.updates = Default::default();

        // Update assignments of (possibly) changed objects.
        for (wref, os) in changed.objects.iter() {
            for oref in os.iter() {
                self.update_object_assignments(oref, Some(wref));
            }
        }

        for tref in changed.tasks.iter() {
            self.update_task_assignment(tref);
        }
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }
}

impl ConsistencyCheck for State {
    /// Check consistency of all tasks, objects, workers, clients and sessions. Quite slow.
    fn check_consistency(&self) -> Result<()> {
        for tr in self.graph.tasks.values() {
            tr.check_consistency()?;
        }
        for or in self.graph.objects.values() {
            or.check_consistency()?;
        }
        for wr in self.graph.workers.values() {
            wr.check_consistency()?;
        }
        for sr in self.graph.sessions.values() {
            sr.check_consistency()?;
        }
        for cr in self.graph.clients.values() {
            cr.check_consistency()?;
        }
        Ok(())
    }
}

/// Note: No `Drop` impl as a `State` is assumed to live forever.
pub type StateRef = WrappedRcRefCell<State>;

impl StateRef {

    pub fn new(handle: Handle, listen_address: SocketAddr) -> Self {
        let mut s = Self::wrap(State {
            graph: Default::default(),
            need_scheduling: false,
            listen_address: listen_address,
            handle: handle,
            scheduler: Default::default(),
            updates: Default::default(),
            stop_server: false,
            self_ref: None,
        });
        s.get_mut().self_ref = Some(s.clone());
        s
    }


    // TODO: Functional cleanup of code below after structures specification


    pub fn start(&self) {
        let listen_address = self.get().listen_address;
        let handle = self.get().handle.clone();
        let listener = TcpListener::bind(&listen_address, &handle).unwrap();

        let state = self.clone();
        let future = listener
            .incoming()
            .for_each(move |(stream, addr)| {
                state.on_connection(stream, addr);
                Ok(())
            })
            .map_err(|e| {
                panic!("Listening failed {:?}", e);
            });
        handle.spawn(future);
        info!("Start listening on address={}", listen_address);
    }

    /// Main loop State entry. Returns `false` when the server should stop.
    pub fn turn(&self) -> bool {
        let mut state = self.get_mut();

        // TODO: better conditional scheduling
        if !state.updates.is_empty() {
            state.run_scheduler();
        }

        // Assign ready tasks to workers (up to overbook limit)
        state.distribute_tasks();

        !state.stop_server
    }

    fn on_connection(&self, stream: TcpStream, address: SocketAddr) {
        // Handle an incoming connection; spawn gate object for it

        info!("New connection from {}", address);
        stream.set_nodelay(true).unwrap();
        let bootstrap = ::server_capnp::server_bootstrap::ToClient::new(
            ServerBootstrapImpl::new(self, address),
        ).from_server::<::capnp_rpc::Server>();

        let rpc_system = new_rpc_system(stream, Some(bootstrap.client));
        self.get().handle.spawn(rpc_system.map_err(|e| {
            panic!("RPC error: {:?}", e)
        }));
    }

    #[inline]
    pub fn handle(&self) -> Handle {
        self.get().handle.clone()
    }
}
