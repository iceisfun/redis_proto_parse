use std::io::{self, Read};

pub use redis_protocol::{RedisCodec, RedisValue, PubSubEvent, PubSubMessage};

mod redis_protocol {
    use super::*;
    use std::io::{Error, ErrorKind::*};

    use bytes::{Buf, BytesMut};
    use tokio_util::codec::Decoder;

    const PROTO_STRING: u8 = b'$';
    const PROTO_LIST: u8 = b'*';
    const PROTO_INT: u8 = b':';
    const PROTO_OK: u8 = b'+';
    const PROTO_ERROR: u8 = b'-';
    const PROTO_CRLF: &[u8; 2] = &[0x0d, 0x0a];

    #[derive(Debug)]
    pub enum RedisValue {
        String(Vec<u8>),
        Int(i64),
        List(Vec<RedisValue>),
        Ok(String),
        Error(String),
    }

    impl RedisValue {
        fn as_str(&self) -> String {
            match self {
                RedisValue::String(data) => {
                    String::from_utf8_lossy(&data).to_string()
                },
                RedisValue::Int(data) => {
                    format!("{}", data)
                },
                RedisValue::Ok(data) => {
                    format!("{}", data)
                },
                RedisValue::Error(data) => {
                    format!("{}", data)
                },
                RedisValue::List(v) => {
                    if let Some(vv)=v.get(0) {
                        return vv.as_str();
                    }
                    "".into()
                },
            }
        }

        fn take_buffer(self) -> Vec<u8> {
            match self {
                RedisValue::String(data) => {
                    data
                },
                _ => self.as_str().into_bytes()
            }
        }
    }

    pub struct PubSubMessage {
        pub channel_name: String,
        pub channel_pattern: Option<String>,
        pub data: Vec<u8>
    }

    impl PubSubMessage {
        fn to_str(&self) -> Result<String, io::Error> {
            Ok(std::str::from_utf8(&self.data)
            .or(Err(Error::new(
                InvalidData,
                "buffer cannot be represented by a utf8 string",
            )))?
            .into())
        }
    }

    pub enum PubSubEvent {
        Message(PubSubMessage),
        Pong(String),
        List((String, Vec<RedisValue>)),
        String(String),
        Int(i64),
        Ok(String),
        Error(String)
    }

    impl TryFrom<RedisValue> for PubSubEvent {
        type Error = io::Error;

        fn try_from(value: RedisValue) -> Result<Self, io::Error> {
            match value {
                RedisValue::List(v) => {
                    let mut v = v.into_iter();
                    if let Some(message_kind)=v.next() {
                        match message_kind.as_str().as_str() {
                            "message" => {
                                if let (Some(rv_channel), Some(rv_data)) = (v.next(), v.next()) {
                                    return Ok(PubSubEvent::Message(PubSubMessage {
                                        channel_name: rv_channel.as_str(),
                                        channel_pattern: None,
                                        data: rv_data.take_buffer(),
                                    }));
                                };

                                return Err(Error::new(InvalidData, "protocol error - 'message' missing some parameters (expects channel, data)"))
                            },
                            "pmessage" => {
                                if let (Some(rv_pattern), Some(rv_channel), Some(rv_data)) = (v.next(), v.next(), v.next()) {
                                    return Ok(PubSubEvent::Message(PubSubMessage {
                                        channel_name: rv_channel.as_str(),
                                        channel_pattern: Some(rv_pattern.as_str()),
                                        data: rv_data.take_buffer(),
                                    }));
                                };

                                return Err(Error::new(InvalidData, "protocol error - 'message' missing some parameters (expects pattern, channel, data)"))
                            },
                            "subscribe" => {
                                // "subscribe" response can end up here [ "subscribe", "channel_name", RedisValue::Int ]
                                //
                                // todo : is N the number of channels in the list of channels subscribed to, subscribe can take multiple arguments
                                //
                                // https://redis.io/commands/subscribe/ -> for each channel, one message with the first element being the string "subscribe" is pushed as a confirmation that the command succeeded.
                                //
                                // *3
                                // $9
                                // subscribe
                                // $20
                                // groupbroadcast::gpio
                                // :1
                                //
                                return Ok(PubSubEvent::List(("subscribe".into(), v.collect())))
                            },
                            "unsubscribe" => {
                                return Ok(PubSubEvent::List(("unsubscribe".into(), v.collect())))
                            },
                            txt => {
                                return Ok(PubSubEvent::List((txt.into(), v.collect())))
                            }
                        }
                    } else {
                        return Err(Error::new(InvalidData, "pubsub stream error - zero length list - capture stream with socat for bug report"))
                    }
                },
                RedisValue::String(v) => {
                    // ping response can come as a $<l>string
                    //
                    // https://redis.io/commands/ping/ -> Bulk string reply the argument provided, when applicable.
                    //
                    // we don't know the contents of this so just return string?
                    // also, can any other string show up, seems like this could just be Pong(v)
                    //
                    // todo : create test case for bulk string ping reply
                    //
                    return Ok(PubSubEvent::Pong(String::from_utf8_lossy(&v).to_string()))
                    //return Ok(PubSubEvent::String(String::from_utf8_lossy(&v).to_string()))
                },
                RedisValue::Int(v) => {
                    // not expected, can this happen? SUBSCRIBE returns number of channels in [ "SUBSCRIBE", x ] i believe
                    //
                    // todo : confirm Rx message on Tx SUBSCRIBE
                    //
                    return Ok(PubSubEvent::Int(v))
                },
                RedisValue::Ok(v) => {
                    //
                    // https://redis.io/commands/ping/ -> Simple string reply, and specifically PONG, when no argument is provided.
                    //
                    // redis> PING
                    // "PONG"   -> +PONG<crlf>
                    //
                    // todo : create test case for simple string reply
                    //

                    if v=="PONG" {
                        return Ok(PubSubEvent::Pong(v.into()))
                    }

                    return Ok(PubSubEvent::Ok(v))
                },
                RedisValue::Error(v) => {
                    return Ok(PubSubEvent::Error(v))
                },
            }
        }
    }


    pub struct RedisCodec;

    impl Decoder for RedisCodec {
        type Item = RedisValue;
        type Error = io::Error;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            // obtain a new slice pointing to the source
            // mut slices have cursor functionality built
            // into the read implemenation
            let reader = &mut src.as_ref();

            match read_value(reader) {
                Ok(val) => {
                    // have a valid RESP RedisValue
                    src.advance(src.len() - reader.len());
                    Ok(Some(val))
                }
                // if we get an unexpected EOF, we need to wait for more data
                Err(e) if e.kind() == UnexpectedEof => Ok(None),
                Err(e) => Err(e),
            }
        }
    }

    fn read_length(src: &mut &[u8]) -> io::Result<i64> {
        for i in 0.. {
            let Some([l, r]) = src.get(i..i+2) else {
                return Err(Error::new(UnexpectedEof, ""))
            };

            if [*l, *r] == *PROTO_CRLF {
                let value = std::str::from_utf8(&src[..=i])
                    .or(Err(Error::new(
                        InvalidData,
                        "len read failed (not a string)",
                    )))?
                    .trim()
                    .parse()
                    .or(Err(Error::new(
                        InvalidData,
                        "len read failed (string not a number)",
                    )))?;
                take_vec(src, i + 2)?;

                return Ok(value);
            }
        }

        unreachable!()
    }

    fn pop_crlf(src: &mut &[u8]) -> io::Result<()> {
        if take_arr(src)? != *PROTO_CRLF {
            return Err(Error::new(InvalidData, "expected CRLF"));
        }

        Ok(())
    }

    fn read_value(src: &mut &[u8]) -> io::Result<RedisValue> {
        let kind=take_u8(src)?;
        Ok(match kind {
            PROTO_STRING => read_redis_string(src)?,
            PROTO_INT => read_redis_int(src)?,
            PROTO_LIST => read_redis_list(src)?,
            PROTO_OK => RedisValue::Ok(read_redis_generic_crlf_string(src)?),
            PROTO_ERROR => RedisValue::Error(read_redis_generic_crlf_string(src)?),
            _ => return Err(Error::new(InvalidData, "invalid type")),
        })
    }

    fn read_redis_list(src: &mut &[u8]) -> io::Result<RedisValue> {
        let len = read_length(src)?;

        if len == -1 {
            // null list has "*-1\r\n"
            return Ok(RedisValue::List(Vec::new()));
        }

        let mut parts = Vec::with_capacity(len as usize);
        for _ in 0..len {
            parts.push(read_value(src)?);
        }

        Ok(RedisValue::List(parts))
    }

    fn read_redis_string(src: &mut &[u8]) -> io::Result<RedisValue> {
        let string_length = read_length(src)?;

        if string_length == -1 {
            // "null" string has "$-1\r\n"
            return Ok(RedisValue::String("".into()));
        }

        let buf = take_vec(src, string_length as usize)?;
        pop_crlf(src)?;

        // Note - this is a raw buffer of non utf8 values, afaik rust "String" wants valid utf8
        Ok(RedisValue::String(buf))
    }

    fn read_redis_int(src: &mut &[u8]) -> io::Result<RedisValue> {
        Ok(RedisValue::Int(read_length(src)?))
    }

    fn read_redis_generic_crlf_string(src: &mut &[u8]) -> io::Result<String> {
        for i in 0.. {
            let Some([l, r]) = src.get(i..i+2) else {
                return Err(Error::new(UnexpectedEof, ""))
            };

            if [*l, *r] == *PROTO_CRLF {
                let value = std::str::from_utf8(&src[..=i])
                    .or(Err(Error::new(
                        InvalidData,
                        "string read failed (not a string)",
                    )))?
                    .trim();
                take_vec(src, i + 2)?;

                return Ok(value.into());
            }
        }

        unreachable!()
    }
}

pub mod resp_stateful_codec {
    use std::io::{Error, ErrorKind::*, self};

    use bytes::{BytesMut, Buf};

    use RedisValue::*;
    use tokio_util::codec::Decoder;

    #[derive(Debug)]
    pub enum RedisValue {
        SimpleString(String),
        Error(String),
        Integer(i64),
        BulkString(Option<Vec<u8>>),
        Array(Option<Vec<RedisValue>>),
    }

    #[derive(Default)]
    struct ArrayContext {
        rem: i64,
        items: Vec<RedisValue>,
    }

    impl ArrayContext {
        fn new(len: i64) -> Self {
            Self {
                rem: len,
                items: Vec::with_capacity(len as usize),
            }
        }

        fn push(&mut self, item: RedisValue) {
            self.items.push(item);

            self.rem -= 1;
            debug_assert!(self.rem >= 0);
        }

        fn is_complete(&self) -> bool {
            self.rem == 0
        }

        fn items(self) -> Vec<RedisValue> {
            self.items
        }
    }

    enum Op {
        SimpleString,
        Error,
        Integer,
        BulkString,
        Array,
    }

    #[derive(Default)]
    pub struct RespDecoder {
        ptr: usize,
        cached_len: Option<i64>,
        doing: Option<Op>,
        stack: Vec<ArrayContext>,
    }

    impl RespDecoder {
        fn get_op(&mut self, src: &mut BytesMut) -> io::Result<Op> {
            let [byte] = *src.split_to(1) else {
                return Err(Error::new(UnexpectedEof, ""))
            };

            let op = match byte {
                b'+' => Op::SimpleString,
                b'-' => Op::Error,
                b':' => Op::Integer,
                b'$' => Op::BulkString,
                b'*' => Op::Array,
                _ => return Err(Error::new(InvalidData, "invalid prefix")),
            };

            Ok(op)
        }

        /// Returns the index of the next CRLF, or an error if EOF is reached
        fn next_crlf(&mut self, src: &mut BytesMut) -> io::Result<usize> {
            loop {
                let crlf = src.get(self.ptr..self.ptr+2)
                    .ok_or(Error::new(UnexpectedEof, ""))?;

                if self.ptr > 512_000_000 {
                    return Err(Error::new(InvalidData, "too long"))
                }

                if crlf == [b'\r', b'\n'] {
                    self.ptr = 0;
                    return Ok(self.ptr)
                };

                self.ptr += 1;
            }
        }

        /// Takes a String and its CRLF delimiter out of the BytesMut instance
        fn inner_string(&mut self, src: &mut BytesMut) -> io::Result<String> {
            let idx = self.next_crlf(src)?;

            // todo: investigate if this can be done without a copy
            let window = src.split_to(idx);
            let slice_as_str = std::str::from_utf8(&window)
                .map_err(|_| Error::new(InvalidData, "invalid utf8"))?;
            
            src.advance(2);
            Ok(slice_as_str.into())
        }

        /// Takes an i64 and its CRLF delimiter out of the BytesMut instance
        fn inner_i32(&mut self, src: &mut BytesMut) -> io::Result<i64> {
            let idx = self.next_crlf(src)?;

            let window = src.split_to(idx);
            let num = std::str::from_utf8(&window)
                .map_err(|_| Error::new(InvalidData, "invalid utf8"))?
                .parse()
                .map_err(|_| Error::new(InvalidData, "invalid integer"))?;

            src.advance(2);
            Ok(num)
        }

        fn get_simple_string(&mut self, src: &mut BytesMut) -> io::Result<RedisValue> {
            Ok(SimpleString(self.inner_string(src)?))
        }

        fn get_error(&mut self, src: &mut BytesMut) -> io::Result<RedisValue> {
            Ok(Error(self.inner_string(src)?))
        }

        fn get_integer(&mut self, src: &mut BytesMut) -> io::Result<RedisValue> {
            Ok(Integer(self.inner_i32(src)?))
        }

        fn get_bulk_string(&mut self, src: &mut BytesMut) -> io::Result<RedisValue> {
            // if the length has already been calculated, use it
            let len = match self.cached_len {
                Some(len) => len,
                None => {
                    let len = self.inner_i32(src)?;

                    if len == -1 {
                        return Ok(BulkString(None))
                    }

                    self.cached_len = Some(len);
                    len
                }
            };
            
            if len > src.len() as i64 {
                return Err(Error::new(UnexpectedEof, ""))
            }

            self.cached_len = None;
            let buf = src.split_to(len as usize).to_vec();

            Ok(BulkString(Some(buf)))
        }

        fn get_array(&mut self, src: &mut BytesMut) -> io::Result<Option<ArrayContext>> {
            let len = self.inner_i32(src)?;

            if len == -1 {
                return Ok(None)
            }

            Ok(Some(ArrayContext::new(len)))
        }

        fn cached_decode(&mut self, src: &mut BytesMut) -> io::Result<RedisValue> {
            loop {
                let Some(op) = &self.doing else {
                    self.doing = Some(self.get_op(src)?);
                    continue
                };

                let mut val = match op {
                    Op::SimpleString => self.get_simple_string(src)?,
                    Op::Error => self.get_error(src)?,
                    Op::Integer => self.get_integer(src)?,
                    Op::BulkString => self.get_bulk_string(src)?,
                    Op::Array => match self.get_array(src)? {
                        None => Array(None),
                        Some(ctx) if ctx.is_complete() => Array(Some(ctx.items())),
                        Some(ctx) => {
                            self.stack.push(ctx);
                            continue
                        },
                    },
                };
                self.doing = None;
    
                loop {
                    let Some(mut ctx) = self.stack.pop() else { return Ok(val) };

                    ctx.push(val);
                    if !ctx.is_complete() {
                        self.stack.push(ctx);
                        break;
                    }

                    val = RedisValue::Array(Some(ctx.items()));
                }   
            }
        }
    }

    impl Decoder for RespDecoder {
        type Item = RedisValue;
        type Error = io::Error;

        fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
            match self.cached_decode(src) {
                // if we get a value, return it
                Ok(val) => Ok(Some(val)),
                // if we get an unexpected EOF, we need to wait for more data
                Err(e) if e.kind() == UnexpectedEof => Ok(None),
                // if we get any other error, we need to return it
                Err(e) => Err(e),
            }
        }
    }
}

fn take_arr<const N: usize>(src: &mut impl Read) -> io::Result<[u8; N]> {
    let mut buf = [0; N];
    src.read_exact(&mut buf)?;
    Ok(buf)
}

fn take_vec(src: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0_u8; n];
    src.read_exact(&mut buf)?;
    Ok(buf)
}

fn take_u8(src: &mut impl Read) -> io::Result<u8> {
    take_arr::<1>(src).map(|[x]| x)
}
