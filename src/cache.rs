use futures::stream::Stream;
use rdkafka::client::EmptyContext;
use rdkafka::config::{ClientConfig, TopicConfig};
use rdkafka::consumer::stream_consumer::StreamConsumer;
use rdkafka::consumer::{Consumer, EmptyConsumerContext};
use rdkafka::producer::FutureProducer;
use rdkafka::error::KafkaError;
use rdkafka::message::Message;
use serde::de::Deserialize;
use serde::ser::Serialize;
use serde_cbor;

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::collections::hash_map;
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use std::thread;

use error::*;
use metadata::{Broker, BrokerId, ClusterId, Group, Partition, TopicName};
use metrics::BrokerMetrics;


#[derive(Serialize, Deserialize, Debug, Hash, Eq, PartialEq)]
struct WrappedKey(String, Vec<u8>);

impl WrappedKey {
    fn new<K>(cache_name: String, key: &K) -> WrappedKey
            where K: Serialize + Deserialize {
        WrappedKey(cache_name, serde_cbor::to_vec(key).unwrap())  //TODO: error handling
    }

    pub fn cache_name(&self) -> &str {
        &self.0
    }

    pub fn serialized_key(&self) -> &[u8] {
        &self.1
    }
}

//
// ********* REPLICA WRITER **********
//

pub struct ReplicaWriter {
    topic_name: String,
    producer: FutureProducer<EmptyContext>,
}

impl ReplicaWriter {
    pub fn new(brokers: &str, topic_name: &str) -> Result<ReplicaWriter> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("compression.codec", "gzip")
            .set("message.max.bytes", "10000000")
            .create::<FutureProducer<_>>()
            .expect("Producer creation error");

        let writer = ReplicaWriter {
            topic_name: topic_name.to_owned(),
            producer: producer,
        };

        Ok(writer)
    }

    // TODO: use structure for value
    pub fn write_update<K, V>(&self, name: &str, key: &K, value: &V) -> Result<()>
            where K: Serialize + Deserialize + Clone,
                  V: Serialize + Deserialize {
        let serialized_key = serde_cbor::to_vec(&WrappedKey::new(name.to_owned(), key))
            .chain_err(|| "Failed to serialize key")?;
        let serialized_value = serde_cbor::to_vec(&value)
            .chain_err(|| "Failed to serialize value")?;
        // trace!("Serialized value size: {}", serialized_value.len());
        trace!("Serialized update size: key={:.3}KB value={:.3}KB",
            (serialized_key.len() as f64 / 1000f64), (serialized_value.len() as f64 / 1000f64));
        let _f = self.producer.send_copy(self.topic_name.as_str(), None, Some(&serialized_value),
                                         Some(&serialized_key), None)
            .chain_err(|| "Failed to produce message")?;
        // _f.wait();  // Uncomment to make production synchronous
        Ok(())
    }
}

//
// ********* REPLICA READER **********
//

#[derive(Debug)]
pub enum ReplicaCacheUpdate<'a> {
    Set { key: &'a[u8], payload: &'a[u8] },
    Delete { key: &'a[u8] }
}

pub trait UpdateReceiver: Send + 'static {
    fn receive_update(&self, name: &str, update: ReplicaCacheUpdate) -> Result<()>;
}

type ReplicaConsumer = StreamConsumer<EmptyConsumerContext>;

pub struct ReplicaReader {
    consumer: ReplicaConsumer,
    brokers: String,
    topic_name: String,
}

impl ReplicaReader {
    pub fn new(brokers: &str, topic_name: &str) -> Result<ReplicaReader> {
        let mut consumer: ReplicaConsumer = ClientConfig::new()
            .set("group.id", "kafka_web_replica_reader")  // TODO: make random
            .set("bootstrap.servers", brokers)
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            //.set("api.version.request", "true")
            .set_default_topic_config(
                TopicConfig::new()
                .set("auto.offset.reset", "smallest")
                .finalize())
            .create()
            .chain_err(|| "Consumer creation failed")?;

        //let topic_partition = TopicPartitionList::with_topics(&vec![topic_name]);
        // consumer.assign(&topic_partition)
        consumer.subscribe(&vec![topic_name])
            .chain_err(|| "Can't subscribe to specified topics")?;

        Ok(ReplicaReader {
            consumer: consumer,
            brokers: brokers.to_owned(),
            topic_name: topic_name.to_owned(),
        })
    }

    pub fn load_state<R: UpdateReceiver>(&mut self, receiver: R) -> Result<()> {
        info!("Started creating state");
        match self.last_message_per_key() {
            Err(e) => format_error_chain!(e),
            Ok(state) => {
                for (w_key, message) in state {
                    let update = match message.payload() {
                        Some(payload) => ReplicaCacheUpdate::Set {
                            key: w_key.serialized_key(),
                            payload: payload
                        },
                        None => ReplicaCacheUpdate::Delete {
                            key: w_key.serialized_key()
                        },
                    };
                    if let Err(e) = receiver.receive_update(w_key.cache_name(), update) {
                        format_error_chain!(e);
                    }
                }
            }
        }
        info!("State creation terminated");
        Ok(())
    }

    fn last_message_per_key(&mut self) -> Result<HashMap<WrappedKey, Message>> {
        let mut eof_set = HashSet::new();
        let mut state: HashMap<WrappedKey, Message> = HashMap::new();

        let topic_name = &self.topic_name;
        let metadata = self.consumer.fetch_metadata(5000)
            .chain_err(|| "Failed to fetch metadata")?;
        let topic_metadata = metadata.topics().iter()
            .find(|m| m.name() == self.topic_name);

        if topic_metadata.is_none() {
            warn!("No replicator topic found ({} {})", self.brokers, self.topic_name);
            return Ok(HashMap::new());
        }
        let topic_metadata = topic_metadata.unwrap();

        for message in self.consumer.start().wait() {
            match message {
                Ok(Ok(m)) => {
                    match parse_message_key(&m).chain_err(|| "Failed to parse message key") {
                        Ok(wrapped_key) => { state.insert(wrapped_key, m); () },
                        Err(e) => format_error_chain!(e),
                    };
                },
                Ok(Err(KafkaError::PartitionEOF(p))) => { eof_set.insert(p); () },
                Ok(Err(e)) => error!("Error while reading from Kafka: {}", e),
                Err(_) => error!("Stream receive error"),
            };
            if eof_set.len() == topic_metadata.partitions().len() {
                self.consumer.stop();
                break;
            }
        }
        Ok(state)
    }
}

fn parse_message_key(message: &Message) -> Result<WrappedKey> {
    let key_bytes = match message.key() {
        Some(k) => k,
        None => bail!("Empty key found"),
    };

    let wrapped_key = serde_cbor::from_slice::<WrappedKey>(key_bytes)
        .chain_err(|| "Failed to decode wrapped key")?;
    Ok(wrapped_key)
}

// pub trait ReplicatedCache {
//     type Key: Serialize + Deserialize;
//     type Value: Serialize + Deserialize;
//
//     fn new(SharedReplicaWriter, &str) -> Self;
//     fn name(&self) -> &str;
//     fn insert(&self, Self::Key, Self::Value) -> Result<Arc<Self::Value>>;
//     fn get(&self, &Self::Key) -> Option<Arc<Self::Value>>;
//     fn keys(&self) -> Vec<Self::Key>;
// }


// impl<V> ValueContainer<V>
//   where K: Serialize + Deserialize {
//     fn new(id: i32, value: V) -> WrappedKey<V> {
//         ValueContainer {
//             id: id,
//             value: value,
//         }
//     }
// }

//
// ********** REPLICATEDMAP **********
//

pub struct ReplicatedMap<K, V>
        where K: Eq + Hash + Clone + Serialize + Deserialize,
              V: Clone + Serialize + Deserialize {
    name: String,
    map: Arc<RwLock<HashMap<K, V>>>,
    replica_writer: Arc<ReplicaWriter>,
}

impl<K, V> ReplicatedMap<K, V> where K: Eq + Hash + Clone + Serialize + Deserialize,
                                     V: Clone + Serialize + Deserialize {
    pub fn new(name: &str, replica_writer: Arc<ReplicaWriter>) -> ReplicatedMap<K, V> {
        ReplicatedMap {
            name: name.to_owned(),
            map: Arc::new(RwLock::new(HashMap::new())),
            replica_writer: replica_writer,
        }
    }

    pub fn alias(&self) -> ReplicatedMap<K, V> {
        ReplicatedMap {
            name: self.name.clone(),
            map: self.map.clone(),
            replica_writer: self.replica_writer.clone(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn keys(&self) -> Vec<K> {
        match self.map.read() {
            Ok(ref cache) => (*cache).keys().cloned().collect::<Vec<_>>(),
            Err(_) => panic!("Poison error"),
        }
    }

    pub fn receive_update(&self, update: ReplicaCacheUpdate) -> Result<()> {
        match update {
            ReplicaCacheUpdate::Set { key, payload } => {
                let key = serde_cbor::from_slice::<K>(&key)
                    .chain_err(|| "Failed to parse key")?;
                let value = serde_cbor::from_slice::<V>(payload)
                    .chain_err(|| "Failed to parse payload")?;
                self.sync_value_update(key, value);
            },
            ReplicaCacheUpdate::Delete { key } => {
                bail!("Delete not implemented");
            }
        }
        Ok(())
    }

    pub fn sync_value_update(&self, key: K, value: V) {
        match self.map.write() {
            Ok(mut cache) => (*cache).insert(key, value),
            Err(_) => panic!("Poison error"),
        };
    }

    pub fn insert(&self, key: K, value: V) -> Result<()> {
        self.replica_writer.write_update(&self.name, &key, &value)
            .chain_err(|| "Failed to write cache update")?;
        self.sync_value_update(key, value);
        Ok(())
    }

    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<V>
        where K: Borrow<Q>,
              Q: Hash + Eq
    {
        match self.map.read() {
            Ok(cache) => { return (*cache).get(key).map(|v| v.clone()) },
            Err(_) => panic!("Poison error"),
        };
    }

    // TODO: add doc
    pub fn lock_iter<F, R>(&self, f: F) -> R
            where F: Fn(hash_map::Iter<K, V>) -> R {
        match self.map.read() {
            Ok(cache) => f(cache.iter()),
            Err(_) => panic!("Poison error"),
        }
    }

    pub fn count<F>(&self, f: F) -> usize
            where F: Fn(&K) -> bool {
        self.lock_iter(|iter| iter.filter(|&(k, _)| f(k)).count())
    }

    pub fn filter_clone<F>(&self, f: F) -> Vec<(K, V)>
            where F: Fn(&K) -> bool {
        self.lock_iter(|iter| {
            iter.filter(|&(k, _)| f(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<(K, V)>>()
        })
    }

    pub fn filter_clone_v<F>(&self, f: F) -> Vec<V>
            where F: Fn(&K) -> bool {
        self.lock_iter(|iter| {
            iter.filter(|&(k, _)| f(k))
                .map(|(_, v)| v.clone())
                .collect::<Vec<V>>()
        })
    }

    pub fn filter_clone_k<F>(&self, f: F) -> Vec<K>
            where F: Fn(&K) -> bool {
        self.lock_iter(|iter| {
            iter.filter(|&(k, _)| f(k))
                .map(|(k, _)| k.clone())
                .collect::<Vec<K>>()
        })
    }
}


//
// ********** CACHE **********
//

pub type MetricsCache = ReplicatedMap<(ClusterId, BrokerId), BrokerMetrics>;
pub type OffsetsCache = ReplicatedMap<(ClusterId, String, TopicName), Vec<i64>>;
pub type BrokerCache = ReplicatedMap<ClusterId, Vec<Broker>>;
pub type TopicCache = ReplicatedMap<(ClusterId, TopicName), Vec<Partition>>;
pub type GroupCache = ReplicatedMap<(ClusterId, String), Group>;


pub struct Cache {
    pub metrics: MetricsCache,
    pub offsets: OffsetsCache,
    pub brokers: BrokerCache,
    pub topics: TopicCache,
    pub groups: GroupCache,
}

impl Cache {
    pub fn new(replica_writer: ReplicaWriter) -> Cache {
        let replica_writer_arc = Arc::new(replica_writer);
        Cache {
            metrics: ReplicatedMap::new("metrics", replica_writer_arc.clone()),
            offsets: ReplicatedMap::new("offsets", replica_writer_arc.clone()),
            brokers: ReplicatedMap::new("brokers", replica_writer_arc.clone()),
            topics: ReplicatedMap::new("topics", replica_writer_arc.clone()),
            groups: ReplicatedMap::new("groups", replica_writer_arc)
        }
    }

    pub fn alias(&self) -> Cache {
        Cache {
            metrics: self.metrics.alias(),
            offsets: self.offsets.alias(),
            brokers: self.brokers.alias(),
            topics: self.topics.alias(),
            groups: self.groups.alias(),
        }
    }
}

impl UpdateReceiver for Cache {
    fn receive_update(&self, cache_name: &str, update: ReplicaCacheUpdate) -> Result<()> {
        match cache_name.as_ref() {
            "metrics" => self.metrics.receive_update(update),
            "offsets" => self.offsets.receive_update(update),
            "brokers" => self.brokers.receive_update(update),
            "topics" => self.topics.receive_update(update),
            "groups" => self.groups.receive_update(update),
            _ => bail!("Unknown cache name: {}", cache_name),
        };
        Ok(())
    }
}

// pub struct Cache<K, V>
//   where K: Eq + Hash + Serialize + Deserialize,
//         V: Serialize + Deserialize {
//     cache_lock: Arc<RwLock<HashMap<K, V>>>,
//     on_insert: Option<Box<Fn(&K, &V)>>,
//     on_delete: Option<Box<Fn(&K, &V)>>
// }
//
// impl<K, V> Cache<K, V>
//   where K: Eq + Hash + Serialize + Deserialize,
//         V: Serialize + Deserialize {
//
//     pub fn new() -> Cache<K, V> {
//         Cache {
//             cache_lock: Arc::new(RwLock::new(HashMap::new())),
//             on_insert: None,
//             on_delete: None,
//         }
//     }
//
//     pub fn set_on_insert<'a, CB: 'static + Fn(&K, &V)>(&'a mut self, cb: CB) -> &'a mut Cache<K, V> {
//         self.on_insert = Some(Box::new(cb));
//         self
//     }
//
//     pub fn insert(&self, key: K, value: V) {
//         self.on_insert.as_ref().map(|f| (f)(&key, &value));
//         match self.cache_lock.write() {
//             Ok(mut cache_ref) => (*cache_ref).insert(key, value),
//             Err(_) => panic!("Poison error!"),
//         };
//     }
// }
