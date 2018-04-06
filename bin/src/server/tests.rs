pub use indradb::tests;
pub use regex::Regex;
use indradb;
use serde::Deserialize;
use serde_json::value::Value as JsonValue;
use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::Duration;
use uuid::Uuid;
use std::sync::Arc;
use grpcio::{Environment, ChannelBuilder, ClientDuplexSender, ClientDuplexReceiver, WriteFlags};
use futures::{Future, Sink, Stream};
use futures::stream::Wait;
use futures::sink::Send;
use std::sync::Mutex;

use request::*;
use edges::EdgeKey;
use vertices::Vertex;
use queries::{EdgeQuery, VertexQuery};
use response::TransactionResponse;
use service_grpc::IndraDbClient;
use converters::ReverseFrom;
use errors;

const START_PORT: usize = 27615;

lazy_static! {
    static ref PORT: AtomicUsize = AtomicUsize::new(START_PORT);
}

pub struct GrpcDatastore {
    port: usize,
    server: Child,
    client: IndraDbClient
}

impl GrpcDatastore {
    fn default() -> Self {
        let port = PORT.fetch_add(1, Ordering::SeqCst);

        let mut envs = HashMap::new();
        envs.insert("PORT", port.to_string());

        let server = Command::new("../target/debug/indradb-server")
            .envs(envs)
            .spawn()
            .expect("Server failed to start");

        let env = Arc::new(Environment::new(1));
        let channel = ChannelBuilder::new(env).connect(&format!("127.0.0.1:{}", port));
        let client = IndraDbClient::new(channel);

        for _ in 0..5 {
            sleep(Duration::from_secs(1));

            let request = PingRequest::new();

            if let Ok(response) = client.ping(&PingRequest::new()) {
                if response.get_ok() {
                    return Self {
                        port: port,
                        server: server,
                        client: client
                    };
                }
            }
        }

        panic!("Server failed to initialize after a few seconds");
    }
}

impl Drop for GrpcDatastore {
    fn drop(&mut self) {
        if let Err(err) = self.server.kill() {
            panic!(format!("Could not kill server instance: {}", err))
        }
    }
}

impl indradb::Datastore<GrpcTransaction> for GrpcDatastore {
    fn transaction(&self) -> Result<GrpcTransaction, indradb::Error> {
        let (sink, receiver) = self.client.transaction().unwrap();
        let channel = GrpcTransactionDuplex::new(sink, receiver.wait());
        Ok(GrpcTransaction::new(channel))
    }
}

struct GrpcTransactionDuplex {
    sink: Option<ClientDuplexSender<TransactionRequest>>,
    receiver: Wait<ClientDuplexReceiver<TransactionResponse>>
}

impl GrpcTransactionDuplex {
    fn new(sink: ClientDuplexSender<TransactionRequest>, receiver: Wait<ClientDuplexReceiver<TransactionResponse>>) -> Self {
        Self {
            sink: Some(sink),
            receiver: receiver,
        }
    }

    fn request(&mut self, req: TransactionRequest) -> Result<TransactionResponse, indradb::Error> {
        let sink: ClientDuplexSender<TransactionRequest> = self.sink.take().unwrap();
        self.sink = Some(sink.send((req, WriteFlags::default())).wait().unwrap());
        let response = self.receiver.next().unwrap().unwrap();

        if response.has_error() {
            Err(response.get_error().into())
        } else {
            Ok(response)
        }
    }
}

pub struct GrpcTransaction {
    channel: Mutex<GrpcTransactionDuplex>
}

impl GrpcTransaction {
    fn new(channel: GrpcTransactionDuplex) -> Self {
        GrpcTransaction {
            channel: Mutex::new(channel)
        }
    }
}

impl indradb::Transaction for GrpcTransaction {
    fn create_vertex(&self, v: &indradb::Vertex) -> Result<bool, indradb::Error> {
        let mut inner = CreateVertexRequest::new();
        inner.set_vertex(Vertex::from(v.clone()));
        let mut request = TransactionRequest::new();
        request.set_create_vertex(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(response.get_ok())
    }

    fn create_vertex_from_type(&self, t: indradb::Type) -> Result<Uuid, indradb::Error> {
        let mut inner = CreateVertexFromTypeRequest::new();
        inner.set_field_type(t.0);
        let mut request = TransactionRequest::new();
        request.set_create_vertex_from_type(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(Uuid::parse_str(response.get_uuid()).unwrap())
    }

    fn get_vertices(&self, q: &indradb::VertexQuery) -> Result<Vec<indradb::Vertex>, indradb::Error> {
        let mut inner = GetVerticesRequest::new();
        inner.set_query(VertexQuery::from(q.clone()));
        let mut request = TransactionRequest::new();
        request.set_get_vertices(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        let vertices: Result<Vec<indradb::Vertex>, errors::Error> = response.get_vertices().get_vertices().into_iter().map(indradb::Vertex::reverse_from).collect();
        Ok(vertices.unwrap())
    }

    fn delete_vertices(&self, q: &indradb::VertexQuery) -> Result<(), indradb::Error> {
        let mut inner = DeleteVerticesRequest::new();
        inner.set_query(VertexQuery::from(q.clone()));
        let mut request = TransactionRequest::new();
        request.set_delete_vertices(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(())
    }

    fn get_vertex_count(&self) -> Result<u64, indradb::Error> {
        let mut request = TransactionRequest::new();
        request.set_get_vertex_count(GetVertexCountRequest::new());
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(response.get_count())
    }

    fn create_edge(&self, e: &indradb::EdgeKey) -> Result<bool, indradb::Error> {
        let mut inner = CreateEdgeRequest::new();
        inner.set_key(EdgeKey::from(e.clone()));
        let mut request = TransactionRequest::new();
        request.set_create_edge(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(response.get_ok())
    }

    fn get_edges(&self, q: &indradb::EdgeQuery) -> Result<Vec<indradb::Edge>, indradb::Error> {
        let mut inner = GetEdgesRequest::new();
        inner.set_query(EdgeQuery::from(q.clone()));
        let mut request = TransactionRequest::new();
        request.set_get_edges(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        let vertices: Result<Vec<indradb::Edge>, errors::Error> = response.get_edges().get_edges().into_iter().map(indradb::Edge::reverse_from).collect();
        Ok(vertices.unwrap())
    }

    fn delete_edges(&self, q: &indradb::EdgeQuery) -> Result<(), indradb::Error> {
        let mut inner = DeleteEdgesRequest::new();
        inner.set_query(EdgeQuery::from(q.clone()));
        let mut request = TransactionRequest::new();
        request.set_delete_edges(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(())
    }

    fn get_edge_count(&self, id: Uuid, type_filter: Option<&indradb::Type>, direction: indradb::EdgeDirection) -> Result<u64, indradb::Error> {
        let mut inner = GetEdgeCountRequest::new();
        inner.set_id(id.hyphenated().to_string());

        if let Some(type_filter) = type_filter {
            inner.set_type_filter(type_filter.0.clone());
        }

        inner.set_direction(String::from(direction));
        let mut request = TransactionRequest::new();
        request.set_get_edge_count(inner);
        let response = self.channel.lock().unwrap().request(request)?;
        Ok(response.get_count())
    }

    fn get_global_metadata(&self, name: &str) -> Result<Option<JsonValue>, indradb::Error> {
        // self.request(&json!({
        //     "action": "get_global_metadata",
        //     "name": name
        // }))
        unimplemented!();
    }

    fn set_global_metadata(&self, name: &str, value: &JsonValue) -> Result<(), indradb::Error> {
        // self.request(&json!({
        //     "action": "set_global_metadata",
        //     "name": name,
        //     "value": value
        // }))
        unimplemented!();
    }

    fn delete_global_metadata(&self, name: &str) -> Result<(), indradb::Error> {
        // self.request(&json!({
        //     "action": "delete_global_metadata",
        //     "name": name
        // }))
        unimplemented!();
    }

    fn get_vertex_metadata(&self, q: &indradb::VertexQuery, name: &str) -> Result<Vec<indradb::VertexMetadata>, indradb::Error> {
        // self.request(&json!({
        //     "action": "get_vertex_metadata",
        //     "query": q,
        //     "name": name
        // }))
        unimplemented!();
    }

    fn set_vertex_metadata(&self, q: &indradb::VertexQuery, name: &str, value: &JsonValue) -> Result<(), indradb::Error> {
        // self.request(&json!({
        //     "action": "set_vertex_metadata",
        //     "query": q,
        //     "name": name,
        //     "value": value
        // }))
        unimplemented!();
    }

    fn delete_vertex_metadata(&self, q: &indradb::VertexQuery, name: &str) -> Result<(), indradb::Error> {
        // self.request(&json!({
        //     "action": "delete_vertex_metadata",
        //     "query": q,
        //     "name": name
        // }))
        unimplemented!();
    }

    fn get_edge_metadata(&self, q: &indradb::EdgeQuery, name: &str) -> Result<Vec<indradb::EdgeMetadata>, indradb::Error> {
        // self.request(&json!({
        //     "action": "get_edge_metadata",
        //     "query": q,
        //     "name": name
        // }))
        unimplemented!();
    }

    fn set_edge_metadata(&self, q: &indradb::EdgeQuery, name: &str, value: &JsonValue) -> Result<(), indradb::Error> {
        // self.request(&json!({
        //     "action": "set_edge_metadata",
        //     "query": q,
        //     "name": name,
        //     "value": value
        // }))
        unimplemented!();
    }

    fn delete_edge_metadata(&self, q: &indradb::EdgeQuery, name: &str) -> Result<(), indradb::Error> {
        // self.request(&json!({
        //     "action": "delete_edge_metadata",
        //     "query": q,
        //     "name": name
        // }))
        unimplemented!();
    }
}

full_test_impl!({ GrpcDatastore::default() });