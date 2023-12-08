#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::result::Result as CoreResult;
use core::str::Utf8Error;
use defmt::*;
use defmt::{assert, assert_eq};
use embassy_executor::{SpawnError, Spawner};
use embassy_net::dns::{self, DnsQueryType};
use embassy_net::tcp::{self, ConnectError, TcpSocket};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Ipv4Cidr, Stack, StackResources};
use embassy_stm32::eth::generic_smi::GenericSMI;
use embassy_stm32::eth::{Ethernet, PacketQueue};
use embassy_stm32::peripherals::ETH;
use embassy_stm32::rng::Rng;
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, eth, peripherals, rng, Config};

use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, mutex::Mutex};

use embassy_time::Timer;
use embassy_time::{Duration, Instant};
use embedded_io_async::Write;
use heapless;
use no_std_embedded_demo as lib;
use pki_types::{DnsName, InvalidDnsNameError, ServerName};
use rustls::client::{ClientConnectionData, EarlyDataError, UnbufferedClientConnection};
#[allow(unused_imports)]
use rustls::version::{TLS12, TLS13};
use rustls::{
    AppDataRecord, ClientConfig, ConnectionState, EncodeError, EncryptError, InsufficientSizeError,
    MayEncryptAppData, RootCertStore, UnbufferedStatus,
};
use static_cell::make_static;
use {defmt_rtt as _, panic_probe as _};

use crate::buffer::TlsBuffer;
use crate::buffers::Buffers;

bind_interrupts!(struct Irqs {
    ETH => eth::InterruptHandler;
    HASH_RNG => rng::InterruptHandler<peripherals::RNG>;
});

const KB: usize = 1024;
// Note that some sites like www.rust-lang.org will need extra heap allocation here!
const HEAP_SIZE: usize = 25 * KB;
const INCOMING_TLS_BUFSIZ: usize = 6 * KB;
const MAC_ADDR: [u8; 6] = [0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];

const MAX_ITERATIONS: usize = 5 * 20;
const SEND_EARLY_DATA: bool = false;
const EARLY_DATA: &[u8] = b"hello";

const OUTGOING_TLS_BUFSIZ: usize = KB / 2;
const TCP_RX_BUFSIZ: usize = KB;
const TCP_TX_BUFSIZ: usize = KB / 2;
const UNIX_TIME: u64 = 1701460379; // `date +%s`
static NTP_TIME: Mutex<ThreadModeRawMutex, Option<u64>> = Mutex::new(None);
static TIME_FROM_START: Mutex<ThreadModeRawMutex, Option<Instant>> = Mutex::new(None);

const SERVER_NAME: &str = "github.com";
//const SERVER_ADDR: Ipv4Address = Ipv4Address([93, 184, 216, 34]); // example.com

const SERVER_PORT: u16 = 443;

#[embassy_executor::main]
async fn start(spawner: Spawner) -> ! {
    heap::init();

    if let Err(e) = main(&spawner, buffers::get().unwrap()).await {
        error!("{}", Debug2Format(&e));
    }

    info!("Sleeping...");
    loop {
        Timer::after_secs(1).await;
    }
}

async fn main(
    spawner: &Spawner,
    Buffers {
        incoming_tls,
        outgoing_tls,
        tcp_rx,
        tcp_tx,
    }: Buffers,
) -> Result<()> {
    let stack = set_up_network_stack(spawner).await?;

    init_call_to_ntp_server(stack).await;

    TIME_FROM_START
        .lock()
        .await
        .replace(Instant::now());

    info!("querying host {:?}...", SERVER_NAME);
    let dns_results = stack
        .dns_query(SERVER_NAME, DnsQueryType::A)
        .await?;

    let dns_addr: IpAddress = dns_results
        .first()
        .ok_or(Error::NoDnsResolution)?
        .clone();

    let mut socket = TcpSocket::new(stack, tcp_rx, tcp_tx);

    socket.set_timeout(Some(Duration::from_secs(5)));

    info!("Connecting...");

    socket
        .connect((dns_addr, SERVER_PORT))
        .await?;
    info!("Connected to {}", socket.remote_endpoint());

    let mut root_store = RootCertStore::empty();
    root_store.extend(
        webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned(),
    );

    let mut tls_config = ClientConfig::builder_with_provider(lib::PROVIDER)
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        // toggle between TLS versions
        .with_protocol_versions(&[&TLS12])
        //.with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.enable_early_data = SEND_EARLY_DATA;
    // Should be enabled by default!
    tls_config.enable_sni = true;
    tls_config.time_provider = time_provider::stub();
    let tls_config = Arc::new(tls_config);

    let mut incoming_tls = TlsBuffer::fixed(incoming_tls);
    let mut outgoing_tls = TlsBuffer::fixed(outgoing_tls);
    converse(
        &mut incoming_tls,
        false,
        tls_config.clone(),
        &mut outgoing_tls,
        &mut socket,
    )
    .await?;

    if SEND_EARLY_DATA {
        warn!("--- second connection ---");
        converse(
            &mut incoming_tls,
            true,
            tls_config,
            &mut outgoing_tls,
            &mut socket,
        )
        .await?;
    }
    Ok(())
}
pub async fn init_call_to_ntp_server(stack: &'static Stack<Ethernet<'static, ETH, GenericSMI>>) {
    // TODO: SPIN once
    let ntp_time = get_time_from_ntp_server(stack).await;
    NTP_TIME.lock().await.replace(ntp_time);
}

pub async fn get_time_from_ntp_server(
    stack: &'static Stack<Ethernet<'static, ETH, GenericSMI>>,
) -> u64 {
    const NTP_PACKET_SIZE: usize = 48;
    const TX_SECONDS: Range<usize> = 40..44;

    // Cloudflare ntp server
    // TODO: dns resolution
    let ntp_server = IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(162, 159, 200, 123)),
        port: 123,
    };

    let mut rx_meta = [PacketMetadata::EMPTY; 16];
    let mut rx_buffer = [0; 6400];
    let mut tx_meta = [PacketMetadata::EMPTY; 16];
    let mut tx_buffer = [0; 6400];
    let mut buf = [0u8; NTP_PACKET_SIZE];

    let mut sock = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );

    sock.bind(45698).unwrap();

    // this magic number means
    // - use NTPv3
    // - we are a client
    buf[0] = 0x1b;
    sock.send_to(&buf, ntp_server)
        .await
        .unwrap();

    let mut response = buf;

    let (read, _ntc_peer) = sock
        .recv_from(&mut response)
        .await
        .unwrap();

    let transmit_seconds = u32::from_be_bytes(response[TX_SECONDS].try_into().unwrap());
    transmit_seconds.into()
}

async fn converse(
    mut incoming_tls: &mut TlsBuffer<'_>,
    send_early_data: bool,
    tls_config: Arc<ClientConfig>,
    mut outgoing_tls: &mut TlsBuffer<'_>,
    mut socket: &mut TcpSocket<'_>,
) -> CoreResult<(), Error> {
    let server_name = ServerName::DnsName(DnsName::try_from(SERVER_NAME)?);

    let mut conn = UnbufferedClientConnection::new(tls_config, server_name)?;

    let mut sent_request = false;
    let mut received_response = false;

    let mut iter_count = 0;
    let mut open_connection = true;
    let mut sent_early_data = false;

    while open_connection {
        iter_count += 1;
        defmt::assert!(iter_count <= MAX_ITERATIONS);
        info!("Iter count: {}", iter_count);
        trace!("{}B in incoming TLS buffer", incoming_tls.used());
        let UnbufferedStatus { mut discard, state } =
            conn.process_tls_records(incoming_tls.filled_mut())?;

        trace!("state: {}", Debug2Format(&state));
        match state {
            ConnectionState::AppDataAvailable(mut state) => {
                while let Some(res) = state.next_record() {
                    let AppDataRecord {
                        discard: new_discard,
                        payload,
                    } = res?;
                    discard += new_discard;

                    if payload.starts_with(b"HTTP") {
                        let response = core::str::from_utf8(payload)?;
                        let header = response
                            .lines()
                            .next()
                            .unwrap_or(response);

                        info!("Payload: {}", header);
                    } else {
                        info!("(.. continued HTTP response ..)");
                    }

                    received_response = true;
                }
            }
            ConnectionState::MustEncodeTlsData(mut state) => {
                try_or_resize_and_retry(
                    |dest| state.encode(dest),
                    |e| {
                        if let EncodeError::InsufficientSize(is) = &e {
                            Ok(*is)
                        } else {
                            Err(e)
                        }
                    },
                    &mut outgoing_tls,
                )?;
            }

            ConnectionState::MustTransmitTlsData(mut state) => {
                if let Some(mut may_encrypt_early_data) = state.may_encrypt_early_data() {
                    let written = try_or_resize_and_retry(
                        |out_buffer| may_encrypt_early_data.encrypt(EARLY_DATA, out_buffer),
                        |e| {
                            if let EarlyDataError::Encrypt(EncryptError::InsufficientSize(is)) = &e
                            {
                                Ok(*is)
                            } else {
                                Err(e.into())
                            }
                        },
                        &mut outgoing_tls,
                    )?;

                    warn!("queued {}B of early data", written);
                    sent_early_data = true;
                }

                if let Some(mut may_encrypt) = state.may_encrypt_app_data() {
                    encrypt_http_request(&mut sent_request, &mut may_encrypt, &mut outgoing_tls);
                }

                send_tls(&mut socket, &mut outgoing_tls).await?;
                state.done();
            }

            ConnectionState::NeedsMoreTlsData { .. } => {
                recv_tls(&mut socket, &mut incoming_tls).await?;
            }

            ConnectionState::TrafficTransit(mut may_encrypt) => {
                if encrypt_http_request(&mut sent_request, &mut may_encrypt, &mut outgoing_tls) {
                    send_tls(&mut socket, &mut outgoing_tls).await?;
                    recv_tls(&mut socket, &mut incoming_tls).await?;
                } else if !received_response {
                    // this happens in the TLS 1.3 case. the app-data was sent in the preceding
                    // `MustTransmitTlsData` state. the server should have already a response which
                    // we can read out from the socket
                    recv_tls(&mut socket, &mut incoming_tls).await?;
                } else {
                    try_or_resize_and_retry(
                        |out_buffer| may_encrypt.queue_close_notify(out_buffer),
                        |e| {
                            if let EncryptError::InsufficientSize(is) = &e {
                                Ok(*is)
                            } else {
                                Err(e.into())
                            }
                        },
                        &mut outgoing_tls,
                    )?;
                    send_tls(&mut socket, &mut outgoing_tls).await?;
                    open_connection = false;
                }
            }
            ConnectionState::ConnectionClosed => {
                open_connection = false;
            }
            state => {
                defmt::todo!("unhandled state: {:?}", Debug2Format(&state))
            }
        }

        incoming_tls.discard(discard);
        iter_count += 1;
    }

    assert!(sent_request);
    assert!(received_response);
    assert_eq!(send_early_data, sent_early_data);

    Ok(())
}

async fn recv_tls<'a>(
    socket: &'a mut TcpSocket<'_>,
    incoming_tls: &'a mut TlsBuffer<'_>,
) -> Result<()> {
    let read = socket
        .read(incoming_tls.unfilled())
        .await?;
    trace!("read {}B of TLS data", read);
    incoming_tls.advance(read);
    Ok(())
}

async fn send_tls<'a>(
    socket: &'a mut TcpSocket<'_>,
    outgoing_tls: &'a mut TlsBuffer<'_>,
) -> Result<()> {
    socket
        .write_all(&outgoing_tls.filled())
        .await?;
    trace!("sent {}B of TLS data", outgoing_tls.used());
    outgoing_tls.clear();
    Ok(())
}

fn encrypt_http_request(
    sent_request: &mut bool,
    may_encrypt: &mut MayEncryptAppData<'_, ClientConnectionData>,
    outgoing_tls: &mut TlsBuffer,
) -> bool {
    if !*sent_request {
        let written = may_encrypt
            .encrypt(&build_http_request(), &mut outgoing_tls.unfilled())
            .expect("encrypted request does not fit in `outgoing_tls`");
        outgoing_tls.advance(written);
        *sent_request = true;
        warn!("queued HTTP request");
        true
    } else {
        false
    }
}

fn try_or_resize_and_retry<E>(
    mut f: impl FnMut(&mut [u8]) -> CoreResult<usize, E>,
    map_err: impl FnOnce(E) -> CoreResult<InsufficientSizeError, E>,
    outgoing: &mut TlsBuffer,
) -> Result<usize>
where
    Error: From<E>,
{
    let written = match f(outgoing.unfilled()) {
        Ok(written) => written,

        Err(e) => {
            let InsufficientSizeError { required_size } = map_err(e)?;
            let new_len = outgoing.used() + required_size;
            outgoing.reserve(new_len - outgoing.capacity());
            trace!("resized `outgoing_tls` buffer to {}B", new_len);

            f(outgoing.unfilled())?
        }
    };

    outgoing.advance(written);

    Ok(written)
}

mod buffer {
    use alloc::vec;
    use alloc::vec::Vec;
    use core::slice::SliceIndex;

    use defmt::trace;

    pub struct TlsBuffer<'a> {
        inner: Inner<'a>,
        used: usize,
    }

    impl<'a> TlsBuffer<'a> {
        pub fn fixed(buf: &'a mut [u8]) -> Self {
            Self {
                inner: Inner::Fixed(buf),
                used: 0,
            }
        }

        #[allow(dead_code)]
        pub fn growable(initial_capacity: usize) -> TlsBuffer<'static> {
            TlsBuffer {
                inner: Inner::Growable(vec![0; initial_capacity]),
                used: 0,
            }
        }

        /// Mark `num_bytes` as being filled with data
        pub fn advance(&mut self, num_bytes: usize) {
            self.used += num_bytes;
        }

        pub fn capacity(&self) -> usize {
            self.inner.capacity()
        }

        pub fn clear(&mut self) {
            self.used = 0;
        }

        /// discards `num_bytes` from the front of the buffer
        pub fn discard(&mut self, num_bytes: usize) {
            if num_bytes == 0 {
                return;
            }

            let used = self.used;
            self.bytes_mut(..)
                .copy_within(num_bytes..used, 0);
            self.used -= num_bytes;

            trace!("discarded {}B", num_bytes);
        }

        pub fn filled(&mut self) -> &[u8] {
            self.bytes(..self.used)
        }

        pub fn filled_mut(&mut self) -> &mut [u8] {
            self.bytes_mut(..self.used)
        }

        pub fn unfilled(&mut self) -> &mut [u8] {
            self.bytes_mut(self.used..)
        }

        pub fn used(&self) -> usize {
            self.used
        }

        pub fn reserve(&mut self, additional_bytes: usize) {
            let new_len = self.inner.capacity() + additional_bytes;
            self.inner.resize(new_len)
        }

        fn bytes_mut<I>(&mut self, index: I) -> &mut I::Output
        where
            I: SliceIndex<[u8]>,
        {
            self.inner
                .bytes_mut()
                .get_mut(index)
                .unwrap()
        }

        fn bytes<I>(&self, index: I) -> &I::Output
        where
            I: SliceIndex<[u8]>,
        {
            self.inner.bytes().get(index).unwrap()
        }
    }

    enum Inner<'a> {
        Fixed(&'a mut [u8]),
        Growable(Vec<u8>),
    }

    impl Inner<'_> {
        fn bytes_mut(&mut self) -> &mut [u8] {
            match self {
                Inner::Fixed(slice) => slice,
                Inner::Growable(vec) => vec,
            }
        }

        fn bytes(&self) -> &[u8] {
            match self {
                Inner::Fixed(slice) => slice,
                Inner::Growable(vec) => vec,
            }
        }

        fn capacity(&self) -> usize {
            match self {
                Inner::Fixed(slice) => slice.len(),
                Inner::Growable(vec) => vec.len(),
            }
        }

        fn resize(&mut self, new_len: usize) {
            match self {
                Inner::Fixed(_) => panic!("not supported by fixed variant"),
                Inner::Growable(vec) => vec.resize(new_len, 0),
            }
        }
    }
}

async fn set_up_network_stack(spawner: &Spawner) -> Result<&'static MyStack> {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::Bypass,
        });
        config.rcc.pll_src = PllSource::HSE;
        config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::DIV4,
            mul: PllMul::MUL180,
            divp: Some(PllPDiv::DIV2), // 8mhz / 4 * 180 / 2 = 180Mhz.
            divq: None,
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV4;
        config.rcc.apb2_pre = APBPrescaler::DIV2;
        config.rcc.sys = Sysclk::PLL1_P;
    }
    let p = embassy_stm32::init(config);

    // Generate random seed.
    let mut rng = Rng::new(p.RNG, Irqs);
    let mut seed = [0; 8];
    let _ = rng.async_fill_bytes(&mut seed).await;
    getrandom::init(rng);
    let seed = u64::from_le_bytes(seed);

    let device = Ethernet::new(
        make_static!(PacketQueue::<16, 16>::new()),
        p.ETH,
        Irqs,
        p.PA1,
        p.PA2,
        p.PC1,
        p.PA7,
        p.PC4,
        p.PC5,
        p.PG13,
        p.PB13,
        p.PG11,
        GenericSMI::new(0),
        MAC_ADDR,
    );

    //let net_config = embassy_net::Config::dhcpv4(Default::default());

    let mut dns_servers = heapless::Vec::new();
    let _ = dns_servers.push(Ipv4Address::new(1, 1, 1, 1).into());

    let net_config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 50, 204), 24),
        dns_servers,
        gateway: Some(Ipv4Address::new(192, 168, 50, 1)),
    });
    //Init network stack
    let stack = &*make_static!(Stack::new(
        device,
        net_config,
        make_static!(StackResources::<3>::new()),
        seed
    ));

    // Launch network task
    spawner.spawn(net_task(stack))?;

    info!("Waiting for DHCP...");
    let static_cfg = wait_for_config(stack).await;

    let local_addr = static_cfg.address.address();
    info!("IP address: {:?}", local_addr);

    Ok(stack)
}

type MyStack = Stack<Ethernet<'static, ETH, GenericSMI>>;

async fn wait_for_config(stack: &'static Stack<Device>) -> embassy_net::StaticConfigV4 {
    loop {
        if let Some(config) = stack.config_v4() {
            return config.clone();
        }
        embassy_futures::yield_now().await;
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<Device>) -> ! {
    stack.run().await
}

type Device = Ethernet<'static, ETH, GenericSMI>;

type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
enum Error {
    Connect(ConnectError),
    Encode(EncodeError),
    InvalidDnsName(InvalidDnsNameError),
    Rustls(rustls::Error),
    Spawn(SpawnError),
    Tcp(tcp::Error),
    EncryptError(rustls::EncryptError),
    Utf8Error(Utf8Error),
    DnsError(dns::Error),
    NoDnsResolution,
    EarlyDataError(EarlyDataError),
}

impl From<EncodeError> for Error {
    fn from(v: EncodeError) -> Self {
        Self::Encode(v)
    }
}

impl From<InvalidDnsNameError> for Error {
    fn from(v: InvalidDnsNameError) -> Self {
        Self::InvalidDnsName(v)
    }
}

impl From<rustls::Error> for Error {
    fn from(v: rustls::Error) -> Self {
        Self::Rustls(v)
    }
}
impl From<Utf8Error> for Error {
    fn from(v: Utf8Error) -> Self {
        Self::Utf8Error(v)
    }
}

impl From<tcp::Error> for Error {
    fn from(v: tcp::Error) -> Self {
        Self::Tcp(v)
    }
}

impl From<ConnectError> for Error {
    fn from(v: ConnectError) -> Self {
        Self::Connect(v)
    }
}

impl From<dns::Error> for Error {
    fn from(v: dns::Error) -> Self {
        Self::DnsError(v)
    }
}

impl From<SpawnError> for Error {
    fn from(v: SpawnError) -> Self {
        Self::Spawn(v)
    }
}

impl From<rustls::EncryptError> for Error {
    fn from(v: rustls::EncryptError) -> Self {
        Self::EncryptError(v)
    }
}

impl From<EarlyDataError> for Error {
    fn from(v: EarlyDataError) -> Self {
        Self::EarlyDataError(v)
    }
}

mod getrandom {
    use embassy_stm32::peripherals::RNG;
    use embassy_stm32::rng::Rng;
    use getrandom::{register_custom_getrandom, Error};
    use spin::mutex::SpinMutex;

    type MyRng = Rng<'static, RNG>;
    static MY_RNG: SpinMutex<Option<MyRng>> = SpinMutex::new(None);

    pub fn init(rng: MyRng) {
        *MY_RNG.lock() = Some(rng);
    }

    fn my_getrandom(dest: &mut [u8]) -> Result<(), Error> {
        let error = Error::UNSUPPORTED; // value is unimportant
        embassy_futures::block_on(
            MY_RNG
                .lock()
                .as_mut()
                .ok_or(error)?
                .async_fill_bytes(dest),
        )
        .map_err(|_| error)
    }

    register_custom_getrandom!(my_getrandom);
}

mod heap {
    use linked_list_allocator::LockedHeap;
    use spin::Once;

    pub fn init() {
        static ONCE: Once = Once::new();

        ONCE.call_once(|| unsafe {
            static mut MEMORY: [u8; super::HEAP_SIZE] = [0; super::HEAP_SIZE];

            HEAP.lock()
                .init(MEMORY.as_mut_ptr(), MEMORY.len())
        });
    }

    #[global_allocator]
    static HEAP: LockedHeap = LockedHeap::empty();
}

mod time_provider {
    use core::time::Duration;

    use defmt::warn;
    use pki_types::UnixTime;
    use rustls::time_provider::{GetCurrentTime, TimeProvider};

    use crate::{NTP_TIME, TIME_FROM_START};
    const TIME_BETWEEN_1900_1970: u64 = 2_208_988_800;

    pub fn stub() -> TimeProvider {
        TimeProvider::new(StubTimeProvider)
    }

    #[derive(Debug)]
    struct StubTimeProvider;

    impl GetCurrentTime for StubTimeProvider {
        fn get_current_time(&self) -> Option<UnixTime> {
            let ntp_time = embassy_futures::block_on(async {
                let provisory = NTP_TIME.lock().await;
                provisory.as_ref().map(|v| *v)
            });

            let time_from_start =
                embassy_futures::block_on(async { *TIME_FROM_START.lock().await });

            let now_from_start = if let Some(now) = time_from_start {
                now
            } else {
                unreachable!();
            };

            // Either the call to NTP server was successful and we can use NTP time ...
            if let Some(now) = ntp_time {
                let now_in_unix = now - TIME_BETWEEN_1900_1970;

                Some(UnixTime::since_unix_epoch(Duration::from_secs(
                    now_in_unix + now_from_start.elapsed().as_secs(),
                )))
            } else {
                // .. or we can use the hardcoded UNIX time
                Some(UnixTime::since_unix_epoch(Duration::from_secs(
                    super::UNIX_TIME,
                )))
            }
        }
    }
}

mod buffers {
    use core::sync::atomic::{self, AtomicBool};

    use super::{INCOMING_TLS_BUFSIZ, OUTGOING_TLS_BUFSIZ, TCP_RX_BUFSIZ, TCP_TX_BUFSIZ};

    pub fn get() -> Option<Buffers> {
        static ONCE: AtomicBool = AtomicBool::new(false);

        let ord = atomic::Ordering::SeqCst;
        if ONCE
            .compare_exchange(false, true, ord, ord)
            .is_ok()
        {
            unsafe {
                Some(Buffers {
                    incoming_tls: {
                        static mut BUF: [u8; INCOMING_TLS_BUFSIZ] = [0; INCOMING_TLS_BUFSIZ];
                        &mut BUF
                    },
                    outgoing_tls: {
                        static mut BUF: [u8; OUTGOING_TLS_BUFSIZ] = [0; OUTGOING_TLS_BUFSIZ];
                        &mut BUF
                    },
                    tcp_tx: {
                        static mut BUF: [u8; TCP_TX_BUFSIZ] = [0; TCP_TX_BUFSIZ];
                        &mut BUF
                    },
                    tcp_rx: {
                        static mut BUF: [u8; TCP_RX_BUFSIZ] = [0; TCP_RX_BUFSIZ];
                        &mut BUF
                    },
                })
            }
        } else {
            None
        }
    }

    pub struct Buffers {
        pub incoming_tls: &'static mut [u8],
        pub outgoing_tls: &'static mut [u8],
        pub tcp_rx: &'static mut [u8],
        pub tcp_tx: &'static mut [u8],
    }
}

fn build_http_request() -> Vec<u8> {
    format!("GET / HTTP/1.1\r\nHost: {SERVER_NAME}\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n").into_bytes()
}
