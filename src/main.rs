use serde::Deserialize;
use serde::{de::DeserializeOwned, Serialize};
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;

#[derive(Default)]
struct Registry {
    map: HashMap<Key, Val>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    uid: u64,
}

struct Val {
    req_resp_id: TypeId,

    req_prod_leaked: *const SyncSender<Message<(), ()>>,

    req_deser: Option<
        unsafe fn(
            UserRequest<'_>,
            *const SyncSender<Message<(), ()>>,
            Arc<SyncSender<Vec<u8>>>,
        ) -> Result<(), ()>,
    >,
}

unsafe fn map_deser<T, U>(
    umsg: UserRequest<'_>,
    req_tx: *const SyncSender<Message<(), ()>>,
    user_resp: Arc<SyncSender<Vec<u8>>>,
) -> Result<(), ()>
where
    T: Serialize + DeserializeOwned + 'static,
    U: Serialize + DeserializeOwned + 'static,
{
    let req_tx = req_tx.cast::<SyncSender<Message<T, U>>>();

    let u_payload: T = postcard::from_bytes(umsg.req_bytes).map_err(drop)?;
    let msg: Message<T, U> = Message {
        msg: u_payload,
        reply: ReplyTo::Userspace {
            nonce: umsg.nonce,
            outgoing: user_resp,
        },
    };

    (*req_tx).send(msg).map_err(drop)
}

impl Registry {
    fn set_konly<T: 'static, U: 'static>(
        &mut self,
        uid: u64,
        kch: &Arc<SyncSender<Message<T, U>>>,
    ) -> Result<(), ()> {
        let key = Key { uid };
        if self.map.contains_key(&key) {
            return Err(());
        }
        self.map.insert(
            key,
            Val {
                req_resp_id: TypeId::of::<(T, U)>(),
                req_prod_leaked: Arc::into_raw(kch.clone()).cast(),
                req_deser: None,
            },
        );
        Ok(())
    }

    fn set<T, U>(&mut self, uid: u64, kch: &Arc<SyncSender<Message<T, U>>>) -> Result<(), ()>
    where
        T: Serialize + DeserializeOwned + 'static,
        U: Serialize + DeserializeOwned + 'static,
    {
        let key = Key { uid };
        if self.map.contains_key(&key) {
            return Err(());
        }
        self.map.insert(
            key,
            Val {
                req_resp_id: TypeId::of::<(T, U)>(),
                req_prod_leaked: Arc::into_raw(kch.clone()).cast(),
                req_deser: Some(map_deser::<T, U>),
            },
        );
        Ok(())
    }

    fn get_userspace(&self, uid: u64) -> Option<&Val> {
        self.map.get(&Key { uid })
    }

    fn get<T: 'static, U: 'static>(&self, uid: u64) -> Option<Arc<SyncSender<Message<T, U>>>> {
        let item = self.map.get(&Key { uid })?;
        if item.req_resp_id != TypeId::of::<(T, U)>() {
            return None;
        }
        unsafe {
            let prod = item.req_prod_leaked.cast();
            Arc::increment_strong_count(prod);
            Some(Arc::from_raw(prod))
        }
    }
}

#[derive(Serialize, Deserialize)]
struct UserRequest<'a> {
    uid: u64,
    nonce: u32,
    #[serde(borrow)]
    req_bytes: &'a [u8],
}

#[derive(Serialize, Deserialize)]
struct UserResponse<U> {
    uid: u64,
    nonce: u32,
    reply: Result<U, ()>,
}

struct Message<T, U> {
    msg: T,
    reply: ReplyTo<U>,
}

enum ReplyTo<U> {
    Kernel(Arc<SyncSender<U>>),
    Userspace {
        nonce: u32,
        outgoing: Arc<SyncSender<Vec<u8>>>,
    },
}

#[derive(Serialize, Deserialize)]
struct AlphaReq {
    foo: u32,
    bar: String,
    baz: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct AlphaResp {
    oof: u32,
}

struct Beta {
    bib: u16,
    bim: Vec<u16>,
    bap: String,
}

fn main() {
    let (a_req_prod, a_req_cons) = sync_channel::<Message<AlphaReq, AlphaResp>>(16);
    let a_req_prod = Arc::new(a_req_prod);
    let (a_resp_prod, a_resp_cons) = sync_channel::<AlphaResp>(16);
    let a_resp_prod = Arc::new(a_resp_prod);
    // let (bprod, bcons) = sync_channel(16);

    let mut registry = Registry::default();
    registry.set(123, &a_req_prod).unwrap();

    assert!(registry.get::<AlphaReq, AlphaResp>(0).is_none());
    assert!(registry.get::<(), ()>(123).is_none());

    // Kernel to Kernel comms
    {
        let a_found: Arc<SyncSender<Message<AlphaReq, AlphaResp>>> = registry.get(123).unwrap();
        a_found
            .send(Message {
                msg: AlphaReq {
                    foo: 111,
                    bar: "alpha".to_string(),
                    baz: vec![6, 7, 8],
                },
                reply: ReplyTo::Kernel(a_resp_prod.clone()),
            })
            .unwrap();

        let a_req_recv = a_req_cons.recv().unwrap();
        assert_eq!(a_req_recv.msg.foo, 111);
        assert_eq!(&a_req_recv.msg.bar, "alpha");
        assert_eq!(&a_req_recv.msg.baz, &[6u8, 7, 8]);

        // now reply
        match a_req_recv.reply {
            ReplyTo::Kernel(ktx) => ktx.send(AlphaResp { oof: 999 }).unwrap(),
            ReplyTo::Userspace { .. } => panic!(),
        }

        let a_resp_recv = a_resp_cons.recv().unwrap();
        assert_eq!(a_resp_recv.oof, 999);
    }

    // Userspace to kernel comms
    {
        let (user_resp_prod, user_resp_cons) = sync_channel::<Vec<u8>>(16);
        let user_resp_prod = Arc::new(user_resp_prod);
        let userspace_msg = postcard::to_stdvec(&AlphaReq {
            foo: 555,
            bar: "This was serialized".into(),
            baz: vec![69, 4, 20],
        }).unwrap();

        let a_found = registry.get_userspace(123).unwrap();
        let deser_fn = a_found.req_deser.unwrap();
        unsafe {
            deser_fn(
                UserRequest { uid: 123, nonce: 666, req_bytes: &&userspace_msg },
                a_found.req_prod_leaked,
                user_resp_prod.clone(),
            ).unwrap();
        }
        let a_req_recv = a_req_cons.recv().unwrap();
        assert_eq!(a_req_recv.msg.foo, 555);
        assert_eq!(&a_req_recv.msg.bar, "This was serialized");
        assert_eq!(&a_req_recv.msg.baz, &[69u8, 4, 20]);

        match a_req_recv.reply {
            ReplyTo::Kernel(_) => panic!(),
            ReplyTo::Userspace { nonce, outgoing } => {
                let rply = postcard::to_stdvec(&UserResponse {
                    uid: 123,
                    nonce,
                    reply: Ok(AlphaResp { oof: 987 }),
                }).unwrap();
                outgoing.send(rply).unwrap();
            },
        }

        let user_resp_recv = user_resp_cons.recv().unwrap();
        let resp_deser: UserResponse<AlphaResp> = postcard::from_bytes(&user_resp_recv).unwrap();
        assert_eq!(resp_deser.uid, 123);
        assert_eq!(resp_deser.nonce, 666);
        assert_eq!(resp_deser.reply.unwrap().oof, 987);
    }
}
