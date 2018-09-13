use bytecodec::{self, ByteCount, Decode, Encode, EncodeExt, Eos, SizedEncode};
use fibers::sync::mpsc;
use futures::{Async, Stream};
use rustun;
use rustun::transport::{RetransmitTransporter, StunTransport, Transport, UnreliableTransport};
use std::fmt;
use std::net::SocketAddr;
use stun_codec::{
    BrokenMessage as StunBrokenMessage, Message as StunMessage,
    MessageDecoder as StunMessageDecoder, MessageEncoder as StunMessageEncoder, TransactionId,
};

use attribute::Attribute;
use channel_data::{ChannelData, ChannelDataDecoder, ChannelDataEncoder};
use Result;

pub fn udp_transporters() -> (StunUdpTransporter, ChannelDataUdpTransporter) {
    panic!()
}

pub fn tcp_transporters() -> (StunTcpTransporter, ChannelDataTcpTransporter) {
    panic!()
}

pub struct Relayer<T: Transport> {
    send_rx: mpsc::Receiver<(SocketAddr, <T::Encoder as Encode>::Item)>,
    recv_tx: mpsc::Sender<(SocketAddr, <T::Decoder as Decode>::Item)>,
}
impl<T: Transport> Relayer<T> {
    pub fn recv_from_handle(&mut self) -> Option<(SocketAddr, <T::Encoder as Encode>::Item)> {
        if let Ok(Async::Ready(Some(item))) = self.send_rx.poll() {
            Some(item)
        } else {
            None
        }
    }

    pub fn send_to_handle(&mut self, item: (SocketAddr, <T::Decoder as Decode>::Item)) {
        let _ = self.recv_tx.send(item);
    }
}
impl<T: Transport> fmt::Debug for Relayer<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Relayer {{ .. }}")
    }
}

pub struct TransporterHandle<T: Transport> {
    send_tx: mpsc::Sender<(SocketAddr, <T::Encoder as Encode>::Item)>,
    recv_rx: mpsc::Receiver<(SocketAddr, <T::Decoder as Decode>::Item)>,
}
impl<T: Transport> TransporterHandle<T> {
    pub fn new() -> (Self, Relayer<T>) {
        let (send_tx, send_rx) = mpsc::channel();
        let (recv_tx, recv_rx) = mpsc::channel();
        let this = TransporterHandle { send_tx, recv_rx };
        let relayer = Relayer { send_rx, recv_tx };
        (this, relayer)
    }
}
impl<T: Transport> Transport for TransporterHandle<T> {
    type Decoder = T::Decoder;
    type Encoder = T::Encoder;

    fn send(&mut self, peer: SocketAddr, message: <Self::Encoder as Encode>::Item) {
        let _ = self.send_tx.send((peer, message));
    }

    fn recv(&mut self) -> Option<(SocketAddr, <Self::Decoder as Decode>::Item)> {
        match self.recv_rx.poll().expect("never fails") {
            Async::NotReady => None,
            Async::Ready(None) => None,
            Async::Ready(Some(item)) => Some(item),
        }
    }

    fn run_once(&mut self) -> Result<bool> {
        Ok(false)
    }
}
impl<T: UnreliableTransport> UnreliableTransport for TransporterHandle<T> {}
impl<T: Transport> fmt::Debug for TransporterHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TransporterHandle {{ .. }}")
    }
}

#[derive(Debug)]
pub struct StunTcpTransporter(TransporterHandle<rustun::transport::StunTcpTransporter<Attribute>>);
impl Transport for StunTcpTransporter {
    type Decoder = StunMessageDecoder<Attribute>;
    type Encoder = StunMessageEncoder<Attribute>;

    fn send(&mut self, peer: SocketAddr, message: <Self::Encoder as Encode>::Item) {
        self.0.send(peer, message);
    }

    fn recv(&mut self) -> Option<(SocketAddr, <Self::Decoder as Decode>::Item)> {
        self.0.recv()
    }

    fn run_once(&mut self) -> Result<bool> {
        track!(self.0.run_once())
    }
}
impl StunTransport<Attribute> for StunTcpTransporter {
    fn finish_transaction(&mut self, _peer: SocketAddr, _transaction_id: TransactionId) {}
}

#[derive(Debug)]
pub struct StunUdpTransporter(
    RetransmitTransporter<
        Attribute,
        TransporterHandle<
            rustun::transport::UdpTransporter<
                StunMessageDecoder<Attribute>,
                StunMessageEncoder<Attribute>,
            >,
        >,
    >,
);
impl Transport for StunUdpTransporter {
    type Decoder = StunMessageDecoder<Attribute>;
    type Encoder = StunMessageEncoder<Attribute>;

    fn send(&mut self, peer: SocketAddr, message: <Self::Encoder as Encode>::Item) {
        self.0.send(peer, message);
    }

    fn recv(&mut self) -> Option<(SocketAddr, <Self::Decoder as Decode>::Item)> {
        self.0.recv()
    }

    fn run_once(&mut self) -> Result<bool> {
        track!(self.0.run_once())
    }
}
impl StunTransport<Attribute> for StunUdpTransporter {
    fn finish_transaction(&mut self, peer: SocketAddr, transaction_id: TransactionId) {
        self.0.finish_transaction(peer, transaction_id);
    }
}

pub type ChannelDataTcpTransporter = ChannelDataTransporter<
    rustun::transport::StunTcpTransporter<Attribute>,
    rustun::transport::TcpTransporter<TurnMessageDecoder, TurnMessageEncoder>,
>;

pub type ChannelDataUdpTransporter = ChannelDataTransporter<
    rustun::transport::UdpTransporter<StunMessageDecoder<Attribute>, StunMessageEncoder<Attribute>>,
    rustun::transport::UdpTransporter<TurnMessageDecoder, TurnMessageEncoder>,
>;

#[derive(Debug)]
pub struct ChannelDataTransporter<S, T>
where
    S: Transport<Decoder = StunMessageDecoder<Attribute>, Encoder = StunMessageEncoder<Attribute>>,
    T: Transport<Decoder = TurnMessageDecoder, Encoder = TurnMessageEncoder>,
{
    relayer: Relayer<S>,
    transporter: T,
}
impl<S, T> Transport for ChannelDataTransporter<S, T>
where
    S: Transport<Decoder = StunMessageDecoder<Attribute>, Encoder = StunMessageEncoder<Attribute>>,
    T: Transport<Decoder = TurnMessageDecoder, Encoder = TurnMessageEncoder>,
{
    type Decoder = ChannelDataDecoder;
    type Encoder = ChannelDataEncoder;

    fn send(&mut self, peer: SocketAddr, message: <Self::Encoder as Encode>::Item) {
        self.transporter
            .send(peer, TurnMessage::ChannelData(message));
    }

    fn recv(&mut self) -> Option<(SocketAddr, <Self::Decoder as Decode>::Item)> {
        match self.transporter.recv() {
            None => None,
            Some((peer, TurnMessage::Stun(x))) => {
                self.relayer.send_to_handle((peer, Ok(x)));
                None
            }
            Some((peer, TurnMessage::BrokenStun(x))) => {
                self.relayer.send_to_handle((peer, Err(x)));
                None
            }
            Some((peer, TurnMessage::ChannelData(x))) => Some((peer, x)),
        }
    }

    fn run_once(&mut self) -> Result<bool> {
        track!(self.transporter.run_once())
    }
}

#[derive(Debug)]
pub enum TurnMessageDecoder {
    Stun(StunMessageDecoder<Attribute>),
    ChannelData(ChannelDataDecoder),
    None,
}
impl Default for TurnMessageDecoder {
    fn default() -> Self {
        TurnMessageDecoder::None
    }
}
impl Decode for TurnMessageDecoder {
    type Item = TurnMessage;

    fn decode(&mut self, buf: &[u8], eos: Eos) -> bytecodec::Result<usize> {
        loop {
            let next = match self {
                TurnMessageDecoder::Stun(x) => return track!(x.decode(buf, eos)),
                TurnMessageDecoder::ChannelData(x) => return track!(x.decode(buf, eos)),
                TurnMessageDecoder::None => match buf.get(0).map(|&b| b >> 6) {
                    None => return Ok(0),
                    Some(0b00) => TurnMessageDecoder::Stun(Default::default()),
                    Some(0b01) => TurnMessageDecoder::ChannelData(Default::default()),
                    Some(prefix) => {
                        track_panic!(
                            bytecodec::ErrorKind::InvalidInput,
                            "Unknown codec: prefix=0b{:b}",
                            prefix
                        );
                    }
                },
            };
            *self = next;
        }
    }

    fn finish_decoding(&mut self) -> bytecodec::Result<Self::Item> {
        let item = match self {
            TurnMessageDecoder::Stun(x) => track!(x.finish_decoding()).map(|x| {
                x.map(TurnMessage::Stun)
                    .unwrap_or_else(TurnMessage::BrokenStun)
            })?,
            TurnMessageDecoder::ChannelData(x) => {
                track!(x.finish_decoding().map(TurnMessage::ChannelData))?
            }
            TurnMessageDecoder::None => track_panic!(bytecodec::ErrorKind::IncompleteDecoding),
        };
        *self = TurnMessageDecoder::None;
        Ok(item)
    }

    fn requiring_bytes(&self) -> ByteCount {
        match self {
            TurnMessageDecoder::Stun(x) => x.requiring_bytes(),
            TurnMessageDecoder::ChannelData(x) => x.requiring_bytes(),
            TurnMessageDecoder::None => ByteCount::Finite(0),
        }
    }
}

//#[derive(Debug)]
pub enum TurnMessageEncoder {
    Stun(StunMessageEncoder<Attribute>),
    ChannelData(ChannelDataEncoder),
    None,
}
impl Default for TurnMessageEncoder {
    fn default() -> Self {
        TurnMessageEncoder::None
    }
}
impl Encode for TurnMessageEncoder {
    type Item = TurnMessage;

    fn encode(&mut self, buf: &mut [u8], eos: Eos) -> bytecodec::Result<usize> {
        match self {
            TurnMessageEncoder::Stun(x) => track!(x.encode(buf, eos)),
            TurnMessageEncoder::ChannelData(x) => track!(x.encode(buf, eos)),
            TurnMessageEncoder::None => Ok(0),
        }
    }

    fn start_encoding(&mut self, item: Self::Item) -> bytecodec::Result<()> {
        if let TurnMessageEncoder::None = *self {
        } else {
            track_panic!(bytecodec::ErrorKind::EncoderFull);
        }
        *self = match item {
            TurnMessage::Stun(t) => track!(EncodeExt::with_item(t).map(TurnMessageEncoder::Stun))?,
            TurnMessage::BrokenStun(t) => {
                track_panic!(bytecodec::ErrorKind::InvalidInput, "{:?}", t);
            }
            TurnMessage::ChannelData(t) => {
                track!(EncodeExt::with_item(t).map(TurnMessageEncoder::ChannelData))?
            }
        };
        Ok(())
    }

    fn requiring_bytes(&self) -> ByteCount {
        ByteCount::Finite(self.exact_requiring_bytes())
    }
}
impl SizedEncode for TurnMessageEncoder {
    fn exact_requiring_bytes(&self) -> u64 {
        match self {
            TurnMessageEncoder::Stun(x) => x.exact_requiring_bytes(),
            TurnMessageEncoder::ChannelData(x) => x.exact_requiring_bytes(),
            TurnMessageEncoder::None => 0,
        }
    }
}

#[derive(Debug)]
pub enum TurnMessage {
    Stun(StunMessage<Attribute>),
    BrokenStun(StunBrokenMessage),
    ChannelData(ChannelData),
}