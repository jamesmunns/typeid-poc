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

struct BetaReq {
    bib: u16,
    bim: Vec<u16>,
    bap: String,
}

struct BetaResp {
    cha: String,
}

fn main() {
    // Create scenario channels. Normally these would be async channels that were created
    // as part of initializing the driver tasks, however this is just a test, so we hold
    // them all in one place.

    //

    // First, create the "alpha" driver members.
    //
    // * a_req_prod: This is the half that "sends to" a driver
    // * a_req_cons: This is the half that receives messages from the above producer, and
    //     is typically held by the task itself, and await'd on.
    let (a_req_prod, a_req_cons) = sync_channel::<Message<AlphaReq, AlphaResp>>(16);
    let a_req_prod = Arc::new(a_req_prod);

    // Then, create the user of the "alpha" driver
    //
    // * a_resp_prod: This is the "producer" for the task using the alpha driver. When
    //     a task is sending requests to the Alpha driver, it will provide a clone
    //     of its' producer as a "reply address"
    // * a_resp_cons: This is the "consumer" for the task using the alpha driver. When
    //     the driver responds to our request, the message will be returned here.
    let (a_resp_prod, a_resp_cons) = sync_channel::<AlphaResp>(16);
    let a_resp_prod = Arc::new(a_resp_prod);

    //

    // Now, we create the "beta" driver members.
    let (b_req_prod, b_req_cons) = sync_channel::<Message<BetaReq, BetaResp>>(16);
    let b_req_prod = Arc::new(b_req_prod);

    // Then, create the user of the "alpha" driver
    //
    // * a_resp_prod: This is the "producer" for the task using the alpha driver. When
    //     a task is sending requests to the Alpha driver, it will provide a clone
    //     of its' producer as a "reply address"
    // * a_resp_cons: This is the "consumer" for the task using the alpha driver. When
    //     the driver responds to our request, the message will be returned here.
    let (b_resp_prod, b_resp_cons) = sync_channel::<BetaResp>(16);
    let b_resp_prod = Arc::new(b_resp_prod);

    //

    //
    // The alpha driver supports both kernel-to-kernel messages, as well as userspace-to-kernel
    // messages, which pass through a serialization/deserialization step when sent between the
    // two entities.
    let mut registry = Registry::default();
    registry.set(123, &a_req_prod).unwrap();

    assert!(registry.get::<AlphaReq, AlphaResp>(0).is_none());
    assert!(registry.get::<(), ()>(123).is_none());

    // Unlike the alpha driver, the "beta" driver messages do not impl Serialize or Deserialize.
    //
    // This means they must register as "konly", or kernel only handlers.
    registry.set_konly(125, &b_req_prod).unwrap();

    assert!(registry.get::<BetaReq, BetaResp>(0).is_none());
    assert!(registry.get::<(), ()>(125).is_none());

    // "Alpha" driver scenario:
    //
    // Kernel to Kernel comms
    {
        // AS THE USER OF DRIVER ALPHA:
        //
        // Get the Alpha driver from the registry
        let a_found: Arc<SyncSender<Message<AlphaReq, AlphaResp>>> = registry.get(123).unwrap();

        // AS THE USER OF DRIVER ALPHA:
        //
        // Send a request to the alpha driver
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

        // AS DRIVER ALPHA:
        //
        // Receive the request that the user just made
        let a_req_recv = a_req_cons.recv().unwrap();
        assert_eq!(a_req_recv.msg.foo, 111);
        assert_eq!(&a_req_recv.msg.bar, "alpha");
        assert_eq!(&a_req_recv.msg.baz, &[6u8, 7, 8]);

        // AS DRIVER ALPHA:
        //
        // now reply to the given (kernel) reply address
        match a_req_recv.reply {
            ReplyTo::Kernel(ktx) => ktx.send(AlphaResp { oof: 999 }).unwrap(),
            ReplyTo::Userspace { .. } => panic!(),
        }

        // AS THE USER OF DRIVER ALPHA:
        //
        // Receive the response from the driver
        let a_resp_recv = a_resp_cons.recv().unwrap();
        assert_eq!(a_resp_recv.oof, 999);
    }

    // Userspace to kernel comms
    {
        // AS THE USER OF DRIVER ALPHA:
        //
        // Create a channel that simulates the kernel-to-userspace ring buffer.
        // In the real code, this would probably be some sort of async ringbuffer
        // which we serialize into, however for the demo, let's just use another
        // SyncChannel that takes Vec<u8>s instead
        let (user_resp_prod, user_resp_cons) = sync_channel::<Vec<u8>>(16);
        let user_resp_prod = Arc::new(user_resp_prod);

        // AS THE USER OF DRIVER ALPHA:
        //
        // Create a serialized message that acts as a request to the Alpha driver
        let userspace_msg = postcard::to_stdvec(&AlphaReq {
            foo: 555,
            bar: "This was serialized".into(),
            baz: vec![69, 4, 20],
        })
        .unwrap();

        // AS KERNEL SIDE RING BUFFER PROCESSOR, USING DRIVER ALPHA:
        //
        // We pretend we just deserialized the header of a userspace message, and
        // would now like to attempt to send it to the proper driver. We retrieve
        // the driver registration based ONLY on the ID of the driver
        let a_found = registry.get_userspace(123).unwrap();

        // AS KERNEL SIDE RING BUFFER PROCESSOR, USING DRIVER ALPHA:
        //
        // We get the function that deserializes the expected message type. Although
        // the function itself is type-erased, it has been monomorphized to only deserialize
        // a given type.
        let deser_fn = a_found.req_deser.unwrap();
        unsafe {
            deser_fn(
                // This is the outer "header" we pretend we just deserialized
                UserRequest {
                    uid: 123,
                    nonce: 666,
                    // This is the actual message payload
                    req_bytes: &userspace_msg,
                },
                // We provide the function with the Producer for Driver Alpha
                a_found.req_prod_leaked,
                // We also provide the function with the ring-buffer's producer of
                // (serialized) Vec<u8> items to be returned to userspace
                user_resp_prod.clone(),
            )
            .unwrap();
        }

        // AS DRIVER ALPHA:
        //
        // We now attempt to process the message that was deserialized and sent to
        // us (if deserialization was successful)
        let a_req_recv = a_req_cons.recv().unwrap();
        assert_eq!(a_req_recv.msg.foo, 555);
        assert_eq!(&a_req_recv.msg.bar, "This was serialized");
        assert_eq!(&a_req_recv.msg.baz, &[69u8, 4, 20]);

        // AS DRIVER ALPHA:
        //
        // Now we reply to the (userspace) user of driver alpha
        match a_req_recv.reply {
            ReplyTo::Kernel(_) => panic!(),
            ReplyTo::Userspace { nonce, outgoing } => {
                let rply = postcard::to_stdvec(&UserResponse {
                    uid: 123,
                    nonce,
                    reply: Ok(AlphaResp { oof: 987 }),
                })
                .unwrap();
                outgoing.send(rply).unwrap();
            }
        }

        // AS THE USER OF DRIVER ALPHA:
        //
        // Process the serialized response we just received, and verify the data.
        let user_resp_recv = user_resp_cons.recv().unwrap();
        let resp_deser: UserResponse<AlphaResp> = postcard::from_bytes(&user_resp_recv).unwrap();
        assert_eq!(resp_deser.uid, 123);
        assert_eq!(resp_deser.nonce, 666);
        assert_eq!(resp_deser.reply.unwrap().oof, 987);
    }

    //

    // "Beta" driver scenario:
    //
    // Kernel to Kernel comms
    {
        // AS THE USER OF DRIVER BETA:
        //
        // Get the Beta driver from the registry
        let b_found: Arc<SyncSender<Message<BetaReq, BetaResp>>> = registry.get(125).unwrap();

        // AS THE USER OF DRIVER BETA:
        //
        // Send a request to the alpha driver
        b_found
            .send(Message {
                msg: BetaReq {
                    bib: 1234,
                    bim: vec![0xff00, 0x1234],
                    bap: "BOOM BOOM POW".into(),
                },
                reply: ReplyTo::Kernel(b_resp_prod.clone()),
            })
            .unwrap();

        // AS DRIVER BETA:
        //
        // Receive the request that the user just made
        let b_req_recv = b_req_cons.recv().unwrap();
        assert_eq!(b_req_recv.msg.bib, 1234);
        assert_eq!(&b_req_recv.msg.bim, &[0xff00u16, 0x1234]);
        assert_eq!(&b_req_recv.msg.bap, "BOOM BOOM POW");

        // AS DRIVER BETA:
        //
        // now reply to the given (kernel) reply address
        match b_req_recv.reply {
            ReplyTo::Kernel(ktx) => ktx
                .send(BetaResp {
                    cha: "WOOORKED".into(),
                })
                .unwrap(),
            ReplyTo::Userspace { .. } => panic!(),
        }

        // AS THE USER OF DRIVER BETA:
        //
        // Receive the response from the driver
        let b_resp_recv = b_resp_cons.recv().unwrap();
        assert_eq!(b_resp_recv.cha, "WOOORKED");
    }

    println!("Done.");
}
