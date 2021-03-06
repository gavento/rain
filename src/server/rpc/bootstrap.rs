use futures::Future;
use std::net::SocketAddr;
use capnp::capability::Promise;
use capnp;

use super::{ClientServiceImpl, WorkerUpstreamImpl};
use common::id::WorkerId;
use common::convert::{FromCapnp, ToCapnp};
use common::resources::Resources;
use server::state::StateRef;
use server_capnp::server_bootstrap;

use CLIENT_PROTOCOL_VERSION;
use WORKER_PROTOCOL_VERSION;

// ServerBootstrap is the entry point of RPC service.
// It is created on the server and provided
// to every incomming connections

pub struct ServerBootstrapImpl {
    state: StateRef,
    registered: bool,    // true if the connection is already registered
    address: SocketAddr, // Remote address of the connection
}

impl ServerBootstrapImpl {
    pub fn new(state: &StateRef, address: SocketAddr) -> Self {
        ServerBootstrapImpl {
            state: state.clone(),
            registered: false,
            address: address,
        }
    }
}

impl Drop for ServerBootstrapImpl {
    fn drop(&mut self) {
        debug!("ServerBootstrap dropped {}", self.address);
    }
}

impl server_bootstrap::Server for ServerBootstrapImpl {
    fn register_as_client(
        &mut self,
        params: server_bootstrap::RegisterAsClientParams,
        mut results: server_bootstrap::RegisterAsClientResults,
    ) -> Promise<(), ::capnp::Error> {
        if self.registered {
            error!("Multiple registration from connection {}", self.address);
            return Promise::err(capnp::Error::failed(format!(
                "Connection already registered"
            )));
        }

        let params = pry!(params.get());

        if params.get_version() != CLIENT_PROTOCOL_VERSION {
            error!("Client protocol mismatch");
            return Promise::err(capnp::Error::failed(format!("Protocol mismatch")));
        }

        self.registered = true;

        let service = ::client_capnp::client_service::ToClient::new(pry!(ClientServiceImpl::new(
            &self.state,
            &self.address
        ))).from_server::<::capnp_rpc::Server>();

        info!("Connection {} registered as client", self.address);
        results.get().set_service(service);
        Promise::ok(())
    }

    fn register_as_worker(
        &mut self,
        params: server_bootstrap::RegisterAsWorkerParams,
        mut results: server_bootstrap::RegisterAsWorkerResults,
    ) -> Promise<(), ::capnp::Error> {
        if self.registered {
            error!("Multiple registration from connection {}", self.address);
            return Promise::err(capnp::Error::failed(format!(
                "Connection already registered"
            )));
        }

        let params = pry!(params.get());

        if params.get_version() != WORKER_PROTOCOL_VERSION {
            error!("Worker protocol mismatch");
            return Promise::err(capnp::Error::failed(format!("Protocol mismatch")));
        }

        self.registered = true;

        // If worker fully specifies its address, then we use it as worker_id
        // otherwise we use announced port number and assign IP address of connection
        let address = WorkerId::from_capnp(&pry!(params.get_address()));
        let worker_id = if address.ip().is_unspecified() {
            SocketAddr::new(self.address.ip(), address.port())
        } else {
            address
        };

        let resources = Resources::from_capnp(&pry!(params.get_resources()));

        info!(
            "Connection {} registered as worker {} with {:?}",
            self.address, worker_id, resources
        );

        let control = pry!(params.get_control());
        let state = self.state.clone();

        // Ask for resources and then create a new worker in server
        let req = control.get_worker_resources_request();
        Promise::from_future(req.send().promise.and_then(move |_| {
            // The order is important here:
            // 1) add worker
            // 2) create upstream
            // reason: upstream drop tries to remove worker
            let worker = pry!(
                state
                    .get_mut()
                    .add_worker(worker_id, Some(control), resources,)
            );
            let upstream = ::worker_capnp::worker_upstream::ToClient::new(
                WorkerUpstreamImpl::new(&state, &worker),
            ).from_server::<::capnp_rpc::Server>();
            results.get().set_upstream(upstream);
            worker_id.to_capnp(&mut results.get().get_worker_id().unwrap());
            Promise::ok(())
        }))
    }
}
