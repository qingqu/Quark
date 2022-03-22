// Copyright (c) 2021 Quark Container Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use lazy_static::lazy_static;
use std::sync::Once;
//use std::path::Path;
//use std::path::PathBuf;
//use nix::unistd::{mkdir, Pid};
//use nix::sys::stat::Mode;

//use containerd_shim::protos::protobuf::{CodedInputStream};
use containerd_shim::TtrpcResult;
use containerd_shim::Error as TtrpcError;
use containerd_shim::api;
use containerd_shim::api::*;
use containerd_shim::TtrpcContext;
use containerd_shim::ExitSignal;
use containerd_shim::Task;
use containerd_shim::util::*;
use containerd_shim::protos::protobuf::{Message, SingularPtrField};
use containerd_shim::protos::protobuf::well_known_types::{Any, Timestamp};

use super::container::*;

use super::super::super::runc::oci::LinuxResources;
use super::super::super::runc::sandbox::sandbox::*;

lazy_static! {
    pub static ref SANDBOX: Mutex<Sandbox> = Mutex::new(Sandbox::default());
}

#[derive(Clone)]
pub struct ShimTask {
    pub containers: Arc<Mutex<HashMap<String, CommonContainer>>>,
    pub namespace: String,
    pub exit: Arc<ExitSignal>,
    pub shutdown: Arc<Once>,
}

impl ShimTask {
    pub fn New(ns: &str, exit: Arc<ExitSignal>) -> Self {
        Self {
            containers: Arc::new(Mutex::new(Default::default())),
            namespace: ns.to_string(),
            exit,
            shutdown: Arc::new(Once::new()),
        }
    }

    pub fn Destroy(&self) -> TtrpcResult<()> {
        let mut containers = self.containers.lock().unwrap();
        for (_, cont) in containers.iter_mut() {
            cont.container.Destroy().map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        }

        return Ok(())
    }

    pub fn WaitAll(containers: Arc<Mutex<HashMap<String, CommonContainer>>>) {
        thread::spawn(move || {
            let client = SANDBOX.lock().unwrap().WaitAll().unwrap();
            loop {
                let resp = match Sandbox::GetWaitAllResp(&client) {
                    Ok(resp) => resp,
                    Err(e) => {
                        error!("ShimTask error with {:?}", e);
                        return;
                    }
                };

                error!("shim WaitAll {:?}", resp);

                Self::Exit(&containers, resp.cid, resp.execId, resp.status as i32)
            }
        });

    }

    pub fn Exit(containers: &Arc<Mutex<HashMap<String, CommonContainer>>>, cid: String, execId: String, status: i32) {
        match containers.lock().unwrap().get_mut(&cid) {
            None => error!("ShimTask::Exit can't find container {}", cid),
            Some(cont) => {
                error!("shim Exit 1 {:?}", cont.init.pid());
                let bundle = cont.bundle.to_string();
                if execId.len() == 0 {
                    // kill all children process if the container has a private PID namespace
                    if cont.should_kill_all_on_exit(&bundle) {
                        error!("shim Exit 3 {:?}", cont.init.pid());
                        cont.kill(None, 9, true).unwrap_or_else(|e| {
                            error!("failed to kill init's children: {}", e)
                        });
                    }
                    // set exit for init process
                    error!("shim Exit 4 {:?}", cont.init.pid());
                    cont.init.common.set_exited(status);

                    // TODO: publish event
                    return
                }

                match cont.processes.get_mut(&execId) {
                    None => {
                        error!("can't find execId {} in container {}", execId, cid)
                    }
                    Some(p) => {
                        p.set_exited(status);
                        // TODO: publish event
                        return
                    }
                }
            }
        }
    }
}

impl Task for ShimTask {
    fn state(&self, _ctx: &TtrpcContext, req: StateRequest) -> TtrpcResult<StateResponse> {
        info!("shim: state request for {:?}", &req);
        let containers = self.containers.lock().unwrap();
        let container = containers.get(req.id.as_str()).ok_or_else(|| {
            TtrpcError::NotFoundError(format!("can not find container by id {}", req.id.as_str()))
        })?;
        let exec_id = req.exec_id.as_str().none_if(|&x| x.is_empty());
        let mut resp = container.state(exec_id)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        resp.pid = 123;
        info!("shim: state resp for {:?}", &resp);
        Ok(resp)
    }

    fn create(
        &self,
        _ctx: &TtrpcContext,
        req: api::CreateTaskRequest,
    ) -> TtrpcResult<api::CreateTaskResponse> {
        info!("shim: Create request for {:?}", &req);
        // Note: Get containers here is for getting the lock,
        // to make sure no other threads manipulate the containers metadata;
        let mut containers = self.containers.lock().unwrap();

        let ns = self.namespace.as_str();
        let id = req.id.as_str();

        let container = ContainerFactory::Create(ns, &req)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        let mut resp = CreateTaskResponse::new();
        resp.pid = container.pid() as u32;

        let mut sandboxLock = SANDBOX.lock().unwrap();
        sandboxLock.ID = container.SandboxId();
        sandboxLock.Pid = container.Pid();

        let len = containers.len();
        if len == 0 { // root container
            Self::WaitAll(self.containers.clone());
        }

        containers.insert(id.to_string(), container);
        info!("Create request for {} returns pid {}", id, resp.pid);
        return Ok(resp);
    }

    fn start(&self, _ctx: &TtrpcContext, req: StartRequest) -> TtrpcResult<StartResponse> {
        info!("shim: Start request for {:?}", &req);
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::NotFoundError(format!("can not find container by id {}", req.get_id()))
        })?;
        let pid = container.start(req.exec_id.as_str().none_if(|&x| x.is_empty()))
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;

        let mut resp = StartResponse::new();
        resp.pid = pid as u32;
        info!("Start request for {:?} returns pid {}", req, resp.get_pid());
        Ok(resp)
    }

    fn delete(&self, _ctx: &TtrpcContext, req: DeleteRequest) -> TtrpcResult<DeleteResponse> {
        info!("shim: Delete request for {:?}", &req);
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::NotFoundError(format!("can not find container by id {}", req.get_id()))
        })?;
        let exec_id_opt = req.get_exec_id().none_if(|x| x.is_empty());
        let (pid, exit_status, exited_at) = container.delete(exec_id_opt)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        if req.get_exec_id().is_empty() {
            containers.remove(req.id.as_str());
        }
        let mut resp = DeleteResponse::new();
        resp.set_exited_at(exited_at);
        resp.set_pid(pid as u32);
        resp.set_exit_status(exit_status);

        info!("shim: Delete resp for {:?}", &resp);
        Ok(resp)
    }

    fn pids(&self, _ctx: &TtrpcContext, req: PidsRequest) -> TtrpcResult<PidsResponse> {
        debug!("shim: Pids request for {:?}", req);
        let containers = self.containers.lock().unwrap();
        let container = containers.get(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;

        let resp = container.pids()
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        debug!("shim: Pids resp for {:?}", resp);
        Ok(resp)
    }

    fn kill(&self, _ctx: &TtrpcContext, req: KillRequest) -> TtrpcResult<Empty> {
        info!("shim: Kill request for {:?}", req);
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::NotFoundError(format!("can not find container by id {}", req.get_id()))
        })?;
        container.kill(
            req.exec_id.as_str().none_if(|&x| x.is_empty()),
            req.signal,
            req.all,
        ).map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        info!("Kill request for {:?} returns successfully", req);
        Ok(Empty::new())
    }

    fn exec(&self, _ctx: &TtrpcContext, req: ExecProcessRequest) -> TtrpcResult<Empty> {
        info!(
            "Exec request for id: {} exec_id: {}",
            req.get_id(),
            req.get_exec_id()
        );
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;
        container.exec(req)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;

        error!("shim::exec end...");
        Ok(Empty::new())
    }

    fn resize_pty(&self, _ctx: &TtrpcContext, req: ResizePtyRequest) -> TtrpcResult<Empty> {
        debug!(
        "shim: Resize pty request for container {}, exec_id: {}",
        &req.id, &req.exec_id
        );
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;
        container.resize_pty(
            req.get_exec_id().none_if(|&x| x.is_empty()),
            req.height,
            req.width,
        ).map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        Ok(Empty::new())
    }

    fn close_io(&self, _ctx: &TtrpcContext, req: CloseIORequest) -> TtrpcResult<Empty> {
        // unnecessary close io here since fd was closed automatically after object was destroyed.
        error!("shim::close_io req {:?}", &req);
        Ok(Empty::new())
    }

    fn update(&self, _ctx: &TtrpcContext, req: UpdateTaskRequest) -> TtrpcResult<Empty> {
        debug!("shim: Update request for {:?}", req);
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;

        let resources: LinuxResources = serde_json::from_slice(req.get_resources().get_value())
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        container.update(&resources)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        Ok(Empty::new())
    }

    fn wait(&self, _ctx: &TtrpcContext, req: WaitRequest) -> TtrpcResult<WaitResponse> {
        error!("shim: Wait request for {:?}", req);
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;
        let exec_id = req.exec_id.as_str().none_if(|&x| x.is_empty());
        let state = container.state(exec_id)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        if state.status != Status::RUNNING && state.status != Status::CREATED {
            let mut resp = WaitResponse::new();
            resp.exit_status = state.exit_status;
            resp.exited_at = state.exited_at;
            info!("Wait request 111 for {:?} status {:?} returns {:?}", req, &state.status, &resp);
            return Ok(resp);
        }
        let rx = container.wait_channel(req.exec_id.as_str().none_if(|&x| x.is_empty()))
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        // release the lock before waiting the channel
        drop(containers);

        rx.recv()
            .expect_err("wait channel should be closed directly");
        // get lock again.
        let mut containers = self.containers.lock().unwrap();
        let container = containers.get_mut(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;
        let (_, code, exited_at) = container.get_exit_info(exec_id)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        let mut resp = WaitResponse::new();
        resp.exit_status = code as u32;
        let mut ts = Timestamp::new();
        if let Some(ea) = exited_at {
            ts.seconds = ea.unix_timestamp();
            ts.nanos = ea.nanosecond() as i32;
        }
        resp.exited_at = SingularPtrField::some(ts);
        //error!("shim: Wait response 2222 for {:?} returns {:?}", req, &resp);
        Ok(resp)
    }

    fn stats(&self, _ctx: &TtrpcContext, req: StatsRequest) -> TtrpcResult<StatsResponse> {
        debug!("shim: Stats request for {:?}", req);
        let containers = self.containers.lock().unwrap();
        let container = containers.get(req.get_id()).ok_or_else(|| {
            TtrpcError::Other(format!("can not find container by id {}", req.get_id()))
        })?;
        let stats = container.stats()
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;

        // marshal to ttrpc Any
        let mut any = Any::new();
        let mut data = Vec::new();
        stats
            .write_to_vec(&mut data)
            .map_err(|e| TtrpcError::Other(format!("{:?}", e)))?;
        any.set_value(data);
        any.set_type_url(stats.descriptor().full_name().to_string());

        let mut resp = StatsResponse::new();
        resp.set_stats(any);
        debug!("shim: Stats resp for {:?}", resp);
        Ok(resp)
    }

    fn shutdown(&self, _ctx: &TtrpcContext, _req: ShutdownRequest) -> TtrpcResult<Empty> {
        debug!("shim: Shutdown request");
        let containers = self.containers.lock().unwrap();
        if containers.len() > 0 {
            return Ok(Empty::new());
        }

        //todo: handle this
        self.shutdown.call_once(|| {
            self.exit.signal();
        });

        debug!("shim: Shutdown finish");
        Ok(Empty::default())
    }
}