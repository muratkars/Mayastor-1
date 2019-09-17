//! High-level replica json-rpc methods.
//!
//! Replica is a logical data volume exported over nvmf (in SPDK terminology
//! an lvol). Here we define methods for easy management of replicas.

use crate::{
    bdev::{bdev_first, bdev_lookup_by_name, Bdev},
    executor::{cb_arg, complete_callback_1},
    jsonrpc::{jsonrpc_register, Code, JsonRpcError, Result},
    nvmf_target,
    pool::Pool,
};
use futures::{
    channel::oneshot,
    future::{self, FutureExt},
};
use rpc::jsonrpc as jsondata;
use spdk_sys::{
    spdk_lvol,
    vbdev_lvol_create_with_uuid,
    vbdev_lvol_destroy,
    vbdev_lvol_get_from_bdev,
    LVOL_CLEAR_WITH_DEFAULT,
};
use std::ffi::{c_void, CStr, CString};

/// Structure representing a replica which is basically SPDK lvol.
///
/// Note about safety: The structure wraps raw C pointer from SPDK.
/// It is safe to use only in synchronous context. If you keep Replica for
/// longer than that then something else can run on reactor_0 inbetween
/// which may destroy the replica and invalidate the pointer!
struct Replica {
    lvol_ptr: *mut spdk_lvol,
}

impl Replica {
    /// Create replica on storage pool.
    pub async fn create(
        uuid: &str,
        pool: &str,
        size: u64,
        thin: bool,
    ) -> Result<Self> {
        let lvs = match Pool::lookup(pool) {
            Some(p) => p.as_ptr(),
            None => {
                return Err(JsonRpcError::new(
                    Code::NotFound,
                    format!("The pool {} does not exist", pool),
                ));
            }
        };
        if Self::lookup(uuid).is_some() {
            return Err(JsonRpcError::new(
                Code::AlreadyExists,
                format!("Replica {} already exists", uuid),
            ));
        }
        let c_uuid = CString::new(uuid).unwrap();
        let (sender, receiver) =
            oneshot::channel::<std::result::Result<*mut spdk_lvol, i32>>();
        let rc = unsafe {
            vbdev_lvol_create_with_uuid(
                lvs,
                c_uuid.as_ptr(),
                size,
                thin,
                // "clear with default" == unmap
                // TODO: what if device does not support unmap, will it get
                // cleared?
                LVOL_CLEAR_WITH_DEFAULT,
                c_uuid.as_ptr(),
                Some(Self::replica_done_cb),
                cb_arg(sender),
            )
        };
        if rc != 0 {
            return Err(JsonRpcError::new(
                Code::InvalidParams,
                format!("Failed to create replica {}", uuid),
            ));
        }

        match receiver.await.expect("Cancellation is not supported") {
            Ok(lvol_ptr) => Ok(Self {
                lvol_ptr,
            }),
            Err(errno) => Err(JsonRpcError::new(
                Code::InvalidParams,
                format!("Failed to create replica {} (errno={})", uuid, errno),
            )),
        }
    }

    /// Lookup replica by uuid (=name).
    pub fn lookup(uuid: &str) -> Option<Replica> {
        match bdev_lookup_by_name(uuid) {
            Some(bdev) => {
                let lvol = unsafe { vbdev_lvol_get_from_bdev(bdev.as_ptr()) };
                if lvol.is_null() {
                    None
                } else {
                    Some(Replica {
                        lvol_ptr: lvol,
                    })
                }
            }
            None => None,
        }
    }

    /// Destroy replica. Consumes the "self" so after calling this method self
    /// can't be used anymore.
    //
    // TODO: Error value should contain self so that it can be used when
    // destroy fails.
    pub async fn destroy(self) -> Result<()> {
        let (sender, receiver) = oneshot::channel::<i32>();
        unsafe {
            vbdev_lvol_destroy(
                self.lvol_ptr,
                Some(complete_callback_1),
                cb_arg(sender),
            );
        }

        let errno = receiver.await.expect("Cancellation is not supported");
        if errno != 0 {
            Err(JsonRpcError::new(
                Code::InternalError,
                format!(
                    "Failed to destroy replica {} (errno={})",
                    self.get_uuid(),
                    errno
                ),
            ))
        } else {
            Ok(())
        }
    }

    /// Expose replica over supported remote access storage protocols (nvmf
    /// and iscsi).
    pub async fn share(&self) -> Result<()> {
        let bdev_ptr = unsafe { (*self.lvol_ptr).bdev };

        match nvmf_target::share(self.get_uuid(), bdev_ptr).await {
            Ok(_) => Ok(()),
            Err(msg) => Err(JsonRpcError::new(
                Code::InternalError,
                format!(
                    "Failed to share replica {} over nvmf: {}",
                    self.get_uuid(),
                    msg
                ),
            )),
        }
    }

    /// The opposite of share.
    pub async fn unshare(&self) -> Result<()> {
        match nvmf_target::unshare(self.get_uuid()).await {
            Ok(_) => Ok(()),
            Err(msg) => Err(JsonRpcError::new(
                Code::InternalError,
                format!(
                    "Failed to unshare replica {}: {}",
                    self.get_uuid(),
                    msg
                ),
            )),
        }
    }

    /// Return a string identifying the share (i.e. for nvmf it is nqn) or
    /// none if the replica is not shared.
    pub fn get_share_id(&self) -> Option<String> {
        nvmf_target::get_nqn(self.get_uuid())
    }

    /// Get size of the replica in bytes.
    pub fn get_size(&self) -> u64 {
        let bdev: Bdev = unsafe { (*self.lvol_ptr).bdev.into() };
        u64::from(bdev.block_size()) * bdev.num_blocks()
    }

    /// Get name of the pool which replica belongs to.
    pub fn get_pool_name(&self) -> &str {
        unsafe {
            let lvs = &*(*self.lvol_ptr).lvol_store;
            CStr::from_ptr(&lvs.name as *const i8).to_str().unwrap()
        }
    }

    /// Get uuid (= name) of the replica.
    pub fn get_uuid(&self) -> &str {
        unsafe {
            CStr::from_ptr(&(*self.lvol_ptr).uuid_str as *const i8)
                .to_str()
                .unwrap()
        }
    }

    /// Return if replica has been thin provisioned.
    pub fn is_thin(&self) -> bool {
        unsafe { (*self.lvol_ptr).thin_provision }
    }

    /// Return raw pointer to lvol (C struct spdk_lvol).
    pub fn as_ptr(&self) -> *mut spdk_lvol {
        self.lvol_ptr
    }

    /// Callback called from SPDK for replica create method.
    extern "C" fn replica_done_cb(
        sender_ptr: *mut c_void,
        lvol_ptr: *mut spdk_lvol,
        errno: i32,
    ) {
        let sender = unsafe {
            Box::from_raw(
                sender_ptr
                    as *mut oneshot::Sender<
                        std::result::Result<*mut spdk_lvol, i32>,
                    >,
            )
        };
        let res = if errno == 0 { Ok(lvol_ptr) } else { Err(errno) };
        sender.send(res).expect("Receiver is gone");
    }
}

/// Iterator over replicas
struct ReplicaIter {
    /// Last bdev examined by the iterator during the call to next()
    bdev: Option<Bdev>,
}

impl ReplicaIter {
    fn new() -> ReplicaIter {
        ReplicaIter {
            bdev: None,
        }
    }
}

impl Iterator for ReplicaIter {
    type Item = Replica;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let maybe_bdev = match &mut self.bdev {
                Some(bdev) => bdev.next(),
                None => bdev_first(),
            };
            let bdev = match maybe_bdev {
                Some(bdev) => bdev,
                None => return None,
            };

            // Skip all other bdevs which are not lvols (i.e. aio)
            let lvol = unsafe { vbdev_lvol_get_from_bdev(bdev.as_ptr()) };
            if !lvol.is_null() {
                let mut aliases = bdev.aliases();
                // each lvol has a first alias of form "pool/lvol-name"
                if !aliases.is_empty() {
                    let alias = aliases.remove(0);
                    let parts: Vec<&str> = alias.split('/').collect();

                    if parts.len() == 2 && bdev.name() == parts[1] {
                        let replica = Replica {
                            lvol_ptr: lvol,
                        };

                        if replica.get_pool_name() == parts[0] {
                            // our lvols have uuid == name
                            if bdev.name() == replica.get_uuid() {
                                // we found a replica
                                self.bdev = Some(bdev);
                                return Some(replica);
                            }
                        }
                    }
                }
            }
            self.bdev = Some(bdev);
        }
    }
}

/// Register replica json-rpc methods.
pub fn register_replica_methods() {
    jsonrpc_register("create_replica", |args: jsondata::CreateReplicaArgs| {
        let fut = async move {
            let replica = Replica::create(
                &args.uuid,
                &args.pool,
                args.size,
                args.thin_provision,
            )
            .await?;

            if args.share == jsondata::ShareProtocol::Nvmf {
                replica.share().await
            } else {
                Ok(())
            }
        };
        fut.boxed_local()
    });

    jsonrpc_register(
        "destroy_replica",
        |args: jsondata::DestroyReplicaArgs| {
            let fut = async move {
                match Replica::lookup(&args.uuid) {
                    Some(replica) => {
                        replica.unshare().await?;
                        replica.destroy().await
                    }
                    None => Err(JsonRpcError::new(
                        Code::NotFound,
                        format!("Replica {} does not exist", args.uuid),
                    )),
                }
            };
            fut.boxed_local()
        },
    );

    jsonrpc_register::<(), _, _>("list_replicas", |_| {
        future::ok(
            ReplicaIter::new()
                .map(|r| jsondata::Replica {
                    uuid: r.get_uuid().to_owned(),
                    pool: r.get_pool_name().to_owned(),
                    size: r.get_size(),
                    thin_provision: r.is_thin(),
                    share: match r.get_share_id() {
                        Some(_) => jsondata::ShareProtocol::Nvmf,
                        None => jsondata::ShareProtocol::None,
                    },
                })
                .collect::<Vec<jsondata::Replica>>(),
        )
        .boxed_local()
    });

    jsonrpc_register::<(), _, _>("stat_replicas", |_| {
        let fut = async {
            let mut stats = Vec::new();

            // XXX is it safe to hold bdev pointer in iterator across context
            // switch!?
            for r in ReplicaIter::new() {
                let lvol = r.as_ptr();
                let uuid = r.get_uuid().to_owned();
                let pool = r.get_pool_name().to_owned();
                let bdev: Bdev = unsafe { (*lvol).bdev.into() };

                // cancelation point here
                let st = bdev.stats().await;

                match st {
                    Ok(st) => {
                        stats.push(jsondata::Stats {
                            uuid,
                            pool,
                            num_read_ops: st.num_read_ops,
                            num_write_ops: st.num_write_ops,
                            bytes_read: st.bytes_read,
                            bytes_written: st.bytes_written,
                        });
                    }
                    Err(errno) => {
                        warn!(
                            "Failed to get stats for {} (errno={})",
                            bdev.name(),
                            errno
                        );
                    }
                }
            }
            Ok(stats)
        };
        fut.boxed_local()
    });
}
