use super::super::{Datastore, Transaction, VertexQuery, EdgeQuery};
use models;
use uuid::Uuid;
use std::collections::{BTreeMap, HashMap, HashSet};
use chrono::DateTime;
use chrono::offset::Utc;
use std::sync::{Arc, RwLock};
use serde_json::Value as JsonValue;
use errors::Error;
use util::{generate_random_secret, parent_uuid, child_uuid};

// All of the data is actually stored in this struct, which is stored
// internally to the datastore itself. This way, we can wrap an rwlock around
// the entire datastore, rather than on a per-data structure basis, as the
// latter approach would risk deadlocking without extreme care.
#[derive(Debug)]
struct InternalMemoryDatastore {
    account_metadata: BTreeMap<(Uuid, String), JsonValue>,
    accounts: HashMap<Uuid, String>,
    edge_metadata: BTreeMap<(models::EdgeKey, String), JsonValue>,
    edges: BTreeMap<models::EdgeKey, (models::Weight, DateTime<Utc>)>,
    global_metadata: BTreeMap<String, JsonValue>,
    vertex_metadata: BTreeMap<(Uuid, String), JsonValue>,
    vertices: BTreeMap<Uuid, models::VertexValue>
}

impl InternalMemoryDatastore {
    fn get_vertex_values_by_query(&self, q: VertexQuery) -> Result<Vec<(Uuid, models::VertexValue)>, Error> {
        match q {
            VertexQuery::All { start_id, limit } => {
                if let Some(start_id) = start_id {
                    Ok(self.vertices.range(start_id..).take(limit as usize).map(|(k, v)| (k.clone(), v.clone())).collect())
                } else {
                    Ok(self.vertices.iter().take(limit as usize).map(|(k, v)| (k.clone(), v.clone())).collect())
                }
            },
            VertexQuery::Vertices { ids } => {
                let mut results = Vec::new();

                for id in ids {
                    let value = self.vertices.get(&id);

                    if let Some(value) = value {
                        results.push((id, value.clone()));
                    }
                }

                Ok(results)
            },
            VertexQuery::Pipe { edge_query, converter, limit } => {
                let edge_values = self.get_edge_values_by_query(*edge_query)?;

                let ids: Vec<Uuid> = match converter.clone() {
                    models::QueryTypeConverter::Outbound => {
                        edge_values.clone().into_iter().take(limit as usize).map(|(key, _, _)| key.outbound_id).collect()
                    },
                    models::QueryTypeConverter::Inbound => {
                        edge_values.clone().into_iter().take(limit as usize).map(|(key, _, _)| key.inbound_id).collect()
                    }
                };

                let mut results = Vec::new();

                for id in ids {
                    let value = self.vertices.get(&id);
                    if let Some(value) = value {
                        results.push((id, value.clone()));
                    }
                }

                Ok(results)
            }
        }
    }

    fn get_edge_values_by_query(&self, q: EdgeQuery) -> Result<Vec<(models::EdgeKey, models::Weight, DateTime<Utc>)>, Error> {
        match q {
            EdgeQuery::Edges { keys } => {
                let mut results = Vec::new();

                for key in keys {
                    let value = self.edges.get(&key);

                    if let Some(&(ref weight, ref update_datetime)) = value {
                        results.push((key, *weight, *update_datetime));
                    }
                }

                Ok(results)
            },
            EdgeQuery::Pipe { vertex_query, converter, type_filter, high_filter, low_filter, limit } => {
                let vertex_values = self.get_vertex_values_by_query(*vertex_query)?;
                let mut results = Vec::new();

                match converter {
                    models::QueryTypeConverter::Outbound => {
                        for (id, _) in vertex_values {
                            let lower_bound = match &type_filter {
                                &Some(ref type_filter) => models::EdgeKey::new(id, type_filter.clone(), Uuid::default()),
                                &None => {
                                    // NOTE: Circumventing the constructor for
                                    // `Type` because it doesn't allow empty
                                    // values, yet we need to use one for
                                    // comparison
                                    let empty_type = models::Type("".to_string());
                                    models::EdgeKey::new(id, empty_type, Uuid::default())
                                }
                            };

                            for (&ref key, &(ref weight, ref update_datetime)) in self.edges.range(lower_bound..) {
                                if key.outbound_id != id {
                                    break;
                                }

                                if let &Some(ref type_filter) = &type_filter {
                                    if &key.t != type_filter {
                                        break;
                                    }
                                }

                                if let Some(high_filter) = high_filter {
                                    if *update_datetime > high_filter {
                                        continue;
                                    }
                                }

                                if let Some(low_filter) = low_filter {
                                    if *update_datetime < low_filter {
                                        continue;
                                    }
                                }

                                results.push((key.clone(), *weight, *update_datetime));

                                if results.len() == limit as usize {
                                    return Ok(results);
                                }
                            }
                        }
                    },
                    models::QueryTypeConverter::Inbound => {
                        let mut candidate_ids = HashSet::new();
                        for (id, _) in vertex_values {
                            candidate_ids.insert(id);
                        }

                        for (&ref key, &(ref weight, ref update_datetime)) in self.edges.iter() {
                            if !candidate_ids.contains(&key.inbound_id) {
                                continue;
                            }

                            if let &Some(ref type_filter) = &type_filter {
                                if &key.t != type_filter {
                                    continue;
                                }
                            }

                            if let Some(high_filter) = high_filter {
                                if *update_datetime > high_filter {
                                    continue;
                                }
                            }

                            if let Some(low_filter) = low_filter {
                                if *update_datetime < low_filter {
                                    continue;
                                }
                            }

                            results.push((key.clone(), *weight, *update_datetime));

                            if results.len() == limit as usize {
                                return Ok(results);
                            }
                        }
                    }
                }

                Ok(results)
            }
        }
    }
}

/// An in-memory-only datastore.
#[derive(Debug)]
pub struct MemoryDatastore(Arc<RwLock<InternalMemoryDatastore>>);

impl MemoryDatastore {
    /// Creates a new in-memory datastore.
    /// 
    /// # Arguments
    /// * `create_default_account` - If set to `true`, a default account with
    ///   a UUID of all 0's and an empty secret will be created. This is
    ///   useful as you oftentimes want to circumvent the entire account \
    ///   system for a simple in-memory-only datastore.
    pub fn new(create_default_account: bool) -> MemoryDatastore {
        let datastore = Self {
            0: Arc::new(RwLock::new(InternalMemoryDatastore {
                account_metadata: BTreeMap::new(),
                accounts: HashMap::new(),
                edge_metadata: BTreeMap::new(),
                edges: BTreeMap::new(),
                global_metadata: BTreeMap::new(),
                vertex_metadata: BTreeMap::new(),
                vertices: BTreeMap::new()
            }))
        };

        if create_default_account {
            datastore.0.write().unwrap().accounts.insert(Uuid::default(), "".to_string());
        }

        datastore
    }
}

impl Datastore<MemoryTransaction> for MemoryDatastore {
    fn has_account(&self, account_id: Uuid) -> Result<bool, Error> {
        Ok(self.0.read().unwrap().accounts.contains_key(&account_id))
    }

    fn create_account(&self) -> Result<(Uuid, String), Error> {
        let id = parent_uuid();
        let secret = generate_random_secret();
        self.0.write().unwrap().accounts.insert(id, secret.clone());
        Ok((id, secret))
    }

    fn delete_account(&self, account_id: Uuid) -> Result<(), Error> {
        if self.0.write().unwrap().accounts.remove(&account_id).is_some() {
            Ok(())
        } else {
            Err(Error::AccountNotFound)
        }
    }

    fn auth(&self, account_id: Uuid, secret: String) -> Result<bool, Error> {
        let datastore = self.0.read().unwrap();
        let fetched_secret = datastore.accounts.get(&account_id);
        Ok(fetched_secret == Some(&secret))
    }

    fn transaction(&self, account_id: Uuid) -> Result<MemoryTransaction, Error> {
        Ok(MemoryTransaction {
            account_id: account_id,
            datastore: self.0.clone()
        })
    }
}

/// A transaction for manipulating in-memory-only datastores.
#[derive(Debug)]
pub struct MemoryTransaction {
    account_id: Uuid,
    datastore: Arc<RwLock<InternalMemoryDatastore>>
}

impl Transaction for MemoryTransaction {
    fn create_vertex(&self, t: models::Type) -> Result<Uuid, Error> {
        let id = child_uuid(self.account_id);
        self.datastore.write().unwrap().vertices.insert(id, models::VertexValue::new(self.account_id, t));
        Ok(id)
    }

    fn get_vertices(&self, q: VertexQuery) -> Result<Vec<models::Vertex>, Error> {
        let vertex_values = self.datastore.read().unwrap().get_vertex_values_by_query(q)?;
        let iter = vertex_values.into_iter().map(|(uuid, value)| models::Vertex::new(uuid, value.t));
        Ok(iter.collect())
    }

    fn delete_vertices(&self, q: VertexQuery) -> Result<(), Error> {
        let vertex_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_vertex_values_by_query(q)?
        };

        let mut datastore = self.datastore.write().unwrap();

        for (uuid, value) in vertex_values.into_iter() {
            if value.owner_id == self.account_id {
                datastore.vertices.remove(&uuid);
            }
        }

        Ok(())
    }

    fn create_edge(&self, key: models::EdgeKey, weight: models::Weight) -> Result<(), Error> {
        {
            let datastore = self.datastore.read().unwrap();
            let value = datastore.vertices.get(&key.outbound_id);

            if let Some(value) = value {
                if value.owner_id != self.account_id {
                    return Err(Error::Unauthorized);
                }

                if !datastore.vertices.contains_key(&key.inbound_id) {
                    return Err(Error::VertexNotFound);
                }
            } else {
                return Err(Error::VertexNotFound);
            }
        }

        let mut datastore = self.datastore.write().unwrap();
        datastore.edges.insert(key, (weight, Utc::now()));
        Ok(())
    }

    fn get_edges(&self, q: EdgeQuery) -> Result<Vec<models::Edge>, Error> {
        let edge_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_edge_values_by_query(q)?
        };

        let iter = edge_values.into_iter().map(|(key, weight, update_datetime)| models::Edge::new(key, weight, update_datetime));
        Ok(iter.collect())
    }

    fn delete_edges(&self, q: EdgeQuery) -> Result<(), Error> {
        let deletable_edges = {
            let datastore = self.datastore.read().unwrap();
            let edge_values = datastore.get_edge_values_by_query(q)?;
            let mut deletable_edges = Vec::new();

            for (key, _, _) in edge_values.into_iter() {
                let vertex_value = datastore.vertices.get(&key.outbound_id).expect(&format!("Expected vertex `{}` to exist", key.outbound_id)[..]);

                if vertex_value.owner_id == self.account_id {
                    deletable_edges.push(key);
                }
            }

            deletable_edges
        };

        let mut datastore = self.datastore.write().unwrap();

        for key in deletable_edges.into_iter() {
            datastore.edges.remove(&key);
        }

        Ok(())
    }

    fn get_edge_count(&self, q: EdgeQuery) -> Result<u64, Error> {
        let edge_values = self.datastore.read().unwrap().get_edge_values_by_query(q)?;
        return Ok(edge_values.len() as u64)
    }

    fn get_global_metadata(&self, name: String) -> Result<JsonValue, Error> {
        let datastore = self.datastore.read().unwrap();
        let value = datastore.global_metadata.get(&name);

        if let Some(value) = value {
            Ok(value.clone())
        } else {
            Err(Error::MetadataNotFound)
        }
    }

    fn set_global_metadata(&self, name: String, value: JsonValue) -> Result<(), Error> {
        let mut datastore = self.datastore.write().unwrap();
        datastore.global_metadata.insert(name, value);
        Ok(())
    }

    fn delete_global_metadata(&self, name: String) -> Result<(), Error> {
        let mut datastore = self.datastore.write().unwrap();
        let value = datastore.global_metadata.remove(&name);
        
        if value.is_some() {
            Ok(())
        } else {
            Err(Error::MetadataNotFound)
        }
    }

    fn get_account_metadata(&self, owner_id: Uuid, name: String) -> Result<JsonValue, Error> {
        let datastore = self.datastore.read().unwrap();
        let value = datastore.account_metadata.get(&(owner_id, name));

        if let Some(value) = value {
            Ok(value.clone())
        } else {
            Err(Error::MetadataNotFound)
        }
    }

    fn set_account_metadata(
        &self,
        owner_id: Uuid,
        name: String,
        value: JsonValue,
    ) -> Result<(), Error> {
        let mut datastore = self.datastore.write().unwrap();
        
        if !datastore.accounts.contains_key(&owner_id) {
            return Err(Error::AccountNotFound);
        }

        datastore.account_metadata.insert((owner_id, name), value);
        Ok(())
    }

    fn delete_account_metadata(&self, owner_id: Uuid, name: String) -> Result<(), Error> {
        let mut datastore = self.datastore.write().unwrap();
        let value = datastore.account_metadata.remove(&(owner_id, name));

        if value.is_some() {
            Ok(())
        } else {
            Err(Error::MetadataNotFound)
        }
    }

    fn get_vertex_metadata(
        &self,
        q: VertexQuery,
        name: String,
    ) -> Result<HashMap<Uuid, JsonValue>, Error> {
        let mut result = HashMap::new();
        let datastore = self.datastore.read().unwrap();
        let vertex_values = datastore.get_vertex_values_by_query(q)?;

        for (id, _) in vertex_values.into_iter() {
            let metadata_value = datastore.vertex_metadata.get(&(id, name.clone()));

            if let Some(metadata_value) = metadata_value {
                result.insert(id, metadata_value.clone());
            }
        }

        Ok(result)
    }

    fn set_vertex_metadata(
        &self,
        q: VertexQuery,
        name: String,
        value: JsonValue,
    ) -> Result<(), Error> {
        let vertex_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_vertex_values_by_query(q)?
        };
        
        let mut datastore = self.datastore.write().unwrap();

        for (id, _) in vertex_values.into_iter() {
            datastore.vertex_metadata.insert((id, name.clone()), value.clone());
        }

        Ok(())
    }

    fn delete_vertex_metadata(&self, q: VertexQuery, name: String) -> Result<(), Error> {
        let vertex_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_vertex_values_by_query(q)?
        };
        
        let mut datastore = self.datastore.write().unwrap();

        for (id, _) in vertex_values.into_iter() {
            datastore.vertex_metadata.remove(&(id, name.clone()));
        }

        Ok(())
    }

    fn get_edge_metadata(
        &self,
        q: EdgeQuery,
        name: String,
    ) -> Result<HashMap<models::EdgeKey, JsonValue>, Error> {
        let mut result = HashMap::new();
        let datastore = self.datastore.read().unwrap();
        let edge_values = datastore.get_edge_values_by_query(q)?;

        for (key, _, _) in edge_values.into_iter() {
            let metadata_value = datastore.edge_metadata.get(&(key.clone(), name.clone()));

            if let Some(metadata_value) = metadata_value {
                result.insert(key, metadata_value.clone());
            }
        }

        Ok(result)
    }

    fn set_edge_metadata(&self, q: EdgeQuery, name: String, value: JsonValue) -> Result<(), Error> {
        let edge_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_edge_values_by_query(q)?
        };
        
        let mut datastore = self.datastore.write().unwrap();

        for (key, _, _) in edge_values.into_iter() {
            datastore.edge_metadata.insert((key, name.clone()), value.clone());
        }

        Ok(())
    }

    fn delete_edge_metadata(&self, q: EdgeQuery, name: String) -> Result<(), Error> {
        let edge_values = {
            let datastore = self.datastore.read().unwrap();
            datastore.get_edge_values_by_query(q)?
        };
        
        let mut datastore = self.datastore.write().unwrap();

        for (key, _, _) in edge_values.into_iter() {
            datastore.edge_metadata.remove(&(key, name.clone()));
        }

        Ok(())
    }

    fn commit(self) -> Result<(), Error> {
        Ok(())
    }

    fn rollback(self) -> Result<(), Error> {
        unimplemented!()
    }
}