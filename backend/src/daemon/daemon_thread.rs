use futures::{
    channel::{mpsc as async_mpsc, oneshot},
    executor::LocalPool,
    future::{abortable, AbortHandle},
    prelude::*,
    task::LocalSpawnExt,
};
use futures_timer::Delay;
use glib::clone;
use once_cell::sync::Lazy;
use std::{
    cell::{Cell, RefCell},
    cmp::PartialEq,
    collections::HashMap,
    hash::{Hash, Hasher},
    rc::Rc,
    sync::{Arc, Mutex, Weak},
    thread::{self, JoinHandle},
    time::Duration,
};

use super::{Benchmark, BoardId, Daemon, Matrix, Nelson, NelsonKind};
use crate::{Board, Bootloaded};

#[derive(Clone, Debug)]
struct Item<K: Hash + Eq, V> {
    key: K,
    value: V,
}

impl<K: Hash + Eq, V> Item<K, V> {
    fn new(key: K, value: V) -> Self {
        Self { key, value }
    }
}

impl<K: Hash + Eq, V> Hash for Item<K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl<K: Hash + Eq, V> PartialEq for Item<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: Hash + Eq, V> Eq for Item<K, V> {}

#[derive(Clone, Hash, Eq, PartialEq, Debug)]
enum SetEnum {
    KeyMap(Item<(BoardId, u8, u8, u8), u16>),
    Color(Item<(BoardId, u8), (u8, u8, u8)>),
    Brightness(Item<(BoardId, u8), i32>),
    Mode(Item<(BoardId, u8), (u8, u8)>),
    Benchmark(BoardId),
    Nelson(BoardId, NelsonKind),
    LedSave(BoardId),
    MatrixGetRate(Item<(), Option<Duration>>),
    Refresh,
    BootLoaderUpdate(Option<Bootloaded>),
    NoInput(BoardId, bool),
    Exit,
}

impl SetEnum {
    fn is_cancelable(&self) -> bool {
        !matches!(self, Self::Nelson(_, _) | Self::Benchmark(_))
    }
}

#[derive(Debug)]
struct Set {
    inner: SetEnum,
    oneshot: oneshot::Sender<Result<Response, String>>,
}

#[derive(Debug)]
enum Response {
    Benchmark(Benchmark),
    Canceled,
    Empty,
    Nelson(Box<Nelson>),
}

impl From<Benchmark> for Response {
    fn from(benchmark: Benchmark) -> Self {
        Response::Benchmark(benchmark)
    }
}

impl From<()> for Response {
    fn from(_unit: ()) -> Self {
        Response::Empty
    }
}

impl From<Nelson> for Response {
    fn from(nelson: Nelson) -> Self {
        Response::Nelson(Box::new(nelson))
    }
}

impl Set {
    fn reply<T: Into<Response>>(self, resp: Result<T, String>) {
        let _ = self.oneshot.send(resp.map(|x| x.into()));
    }
}

pub struct ThreadClient {
    cancels: Mutex<HashMap<SetEnum, AbortHandle>>,
    channel: async_mpsc::UnboundedSender<Set>,
    join_handle: Mutex<Option<JoinHandle<()>>>,
}

impl ThreadClient {
    pub fn new<F: Fn(ThreadResponse) + 'static>(daemon: Box<dyn Daemon>, cb: F) -> Arc<Self> {
        let (sender, reciever) = async_mpsc::unbounded();
        let client = Arc::new(Self {
            cancels: Mutex::new(HashMap::new()),
            channel: sender,
            join_handle: Mutex::new(None),
        });
        let (response_sender, mut response_reciever) = async_mpsc::unbounded();
        glib::MainContext::default().spawn_local(async move {
            while let Some(response) = response_reciever.next().await {
                cb(response)
            }
        });

        let join_handle = Thread::new(daemon, client.clone(), response_sender).spawn(reciever);
        *client.join_handle.lock().unwrap() = Some(join_handle);
        client
    }

    #[allow(clippy::await_holding_lock)]
    async fn send(&self, set_enum: SetEnum) -> Result<Response, String> {
        let mut cancels = self.cancels.lock().unwrap();

        if set_enum.is_cancelable() {
            if let Some(cancel) = cancels.remove(&set_enum) {
                cancel.abort();
            }
        }

        let (sender, receiver) = oneshot::channel();
        let (receiver, cancel) = abortable(receiver);
        cancels.insert(set_enum.clone(), cancel);
        drop(cancels);

        let _ = self.channel.unbounded_send(Set {
            inner: set_enum,
            oneshot: sender,
        });
        match receiver.await {
            Ok(Ok(res)) => res,
            _ => Ok(Response::Canceled),
        }
    }

    async fn send_noresp(&self, set_enum: SetEnum) -> Result<(), String> {
        self.send(set_enum).await.and(Ok(()))
    }

    pub async fn refresh(&self) -> Result<(), String> {
        self.send_noresp(SetEnum::Refresh).await
    }

    pub async fn check_for_bootloader(&self) -> Result<(), String> {
        use regex::bytes::Regex;
        static HAS_USB_HUB: Lazy<Regex> =
            Lazy::new(|| Regex::new("3384:000.*System76 USB").unwrap());
        static ATMEGA32U4: Lazy<Regex> =
            Lazy::new(|| Regex::new("03eb:2ff4.*atmega32u4.*bootloader").unwrap());
        static AT90USB646: Lazy<Regex> =
            Lazy::new(|| Regex::new("03eb:2ff9.*at90usb646.*bootloader").unwrap());

        let lsusb = async_process::Command::new("lsusb")
            .arg("--verbose")
            .output()
            .await
            .map_err(|_| "Failed to run lsusb".to_string())?
            .stdout;

        let update = if AT90USB646.is_match(&lsusb) {
            if HAS_USB_HUB.is_match(&lsusb) {
                Some(Bootloaded::At90usb646)
            } else {
                Some(Bootloaded::At90usb646Lite)
            }
        } else if ATMEGA32U4.is_match(&lsusb) {
            Some(Bootloaded::AtMega32u4)
        } else {
            None
        };

        self.send_noresp(SetEnum::BootLoaderUpdate(update)).await
    }

    pub async fn keymap_set(
        &self,
        board: BoardId,
        layer: u8,
        output: u8,
        input: u8,
        value: u16,
    ) -> Result<(), String> {
        self.send_noresp(SetEnum::KeyMap(Item::new(
            (board, layer, output, input),
            value,
        )))
        .await
    }

    pub async fn set_color(
        &self,
        board: BoardId,
        index: u8,
        color: (u8, u8, u8),
    ) -> Result<(), String> {
        self.send_noresp(SetEnum::Color(Item::new((board, index), color)))
            .await
    }

    pub async fn set_brightness(
        &self,
        board: BoardId,
        index: u8,
        brightness: i32,
    ) -> Result<(), String> {
        self.send_noresp(SetEnum::Brightness(Item::new((board, index), brightness)))
            .await
    }

    pub async fn set_mode(
        &self,
        board: BoardId,
        layer: u8,
        mode: u8,
        speed: u8,
    ) -> Result<(), String> {
        self.send_noresp(SetEnum::Mode(Item::new((board, layer), (mode, speed))))
            .await
    }

    pub async fn set_matrix_get_rate(&self, rate: Option<Duration>) -> Result<(), String> {
        self.send_noresp(SetEnum::MatrixGetRate(Item::new((), rate)))
            .await
    }

    pub async fn benchmark(&self, board: BoardId) -> Result<Benchmark, String> {
        let resp = self.send(SetEnum::Benchmark(board)).await?;
        if let Response::Benchmark(benchmark) = resp {
            Ok(benchmark)
        } else {
            panic!("{}", format!("'{:?}' unexpected", resp));
        }
    }

    pub async fn nelson(&self, board: BoardId, kind: NelsonKind) -> Result<Nelson, String> {
        let resp = self.send(SetEnum::Nelson(board, kind)).await?;
        if let Response::Nelson(nelson) = resp {
            Ok(*nelson)
        } else {
            panic!("{}", format!("'{:?}' unexpected", resp));
        }
    }

    pub async fn led_save(&self, board: BoardId) -> Result<(), String> {
        self.send_noresp(SetEnum::LedSave(board)).await
    }

    pub async fn set_no_input(&self, board: BoardId, no_input: bool) -> Result<(), String> {
        self.send_noresp(SetEnum::NoInput(board, no_input)).await
    }

    pub fn close(&self) {
        let join_handle = match self.join_handle.lock().unwrap().take() {
            Some(join_handle) => join_handle,
            None => {
                return;
            }
        };

        // Send exit command to thread
        let (sender, _receiver) = oneshot::channel();
        let _ = self.channel.unbounded_send(Set {
            inner: SetEnum::Exit,
            oneshot: sender,
        });

        // Wait for thread to terminate
        join_handle.join().unwrap();
    }
}

pub enum ThreadResponse {
    BoardLoading,
    BoardLoadingDone,
    BoardAdded(Board),
    BoardRemoved(BoardId),
    BootloadedAdded(Bootloaded),
    BootloadedRemoved,
}

struct ThreadBoard {
    matrix: Matrix,
    matrix_channel: async_mpsc::UnboundedSender<Matrix>,
    has_matrix: bool,
}

impl ThreadBoard {
    fn new(matrix_channel: async_mpsc::UnboundedSender<Matrix>, has_matrix: bool) -> Self {
        Self {
            matrix: Matrix::default(),
            matrix_channel,
            has_matrix,
        }
    }
}

struct Thread {
    daemon: Box<dyn Daemon>,
    boards: RefCell<HashMap<BoardId, ThreadBoard>>,
    client: Weak<ThreadClient>,
    response_channel: async_mpsc::UnboundedSender<ThreadResponse>,
    matrix_get_rate: Cell<Option<Duration>>,
    previous_bootloaded: RefCell<Option<Bootloaded>>,
    current_bootloaded: RefCell<Option<Bootloaded>>,
}

impl Thread {
    fn new(
        daemon: Box<dyn Daemon>,
        client: Arc<ThreadClient>,
        response_channel: async_mpsc::UnboundedSender<ThreadResponse>,
    ) -> Self {
        Self {
            daemon,
            client: Arc::downgrade(&client),
            response_channel,
            boards: RefCell::new(HashMap::new()),
            matrix_get_rate: Cell::new(None),
            previous_bootloaded: RefCell::new(None),
            current_bootloaded: RefCell::new(None),
        }
    }

    fn spawn(self, mut channel: async_mpsc::UnboundedReceiver<Set>) -> JoinHandle<()> {
        thread::spawn(move || {
            let mut pool = LocalPool::new();
            let spawner = pool.spawner();

            let self_ = Rc::new(self);

            spawner
                .spawn_local(clone!(@strong self_ => async move {
                    loop {
                        if let Some(rate) = self_.matrix_get_rate.get() {
                            Delay::new(rate).await;
                            if let Err(err) = self_.matrix_refresh_all() {
                              error!("{}", err);
                              return
                            }
                        } else {
                            Delay::new(Duration::from_millis(100)).await;
                        }
                    }
                }))
                .unwrap();

            pool.run_until(async move {
                while let Some(set) = channel.next().await {
                    if !self_.handle_set(set) {
                        break;
                    }
                }
            });
        })
    }

    fn handle_set(&self, set: Set) -> bool {
        if set.oneshot.is_canceled() && set.inner != SetEnum::Exit {
            return true;
        }

        match set.inner {
            SetEnum::KeyMap(Item { key, value }) => {
                set.reply(self.daemon.keymap_set(key.0, key.1, key.2, key.3, value))
            }
            SetEnum::Color(Item { key, value }) => {
                set.reply(self.daemon.set_color(key.0, key.1, value))
            }
            SetEnum::Brightness(Item { key, value }) => {
                set.reply(self.daemon.set_brightness(key.0, key.1, value))
            }
            SetEnum::Mode(Item { key, value }) => {
                set.reply(self.daemon.set_mode(key.0, key.1, value.0, value.1))
            }
            SetEnum::Benchmark(board) => set.reply(self.daemon.benchmark(board)),
            SetEnum::Nelson(board, kind) => set.reply(self.daemon.nelson(board, kind)),
            SetEnum::LedSave(board) => set.reply(self.daemon.led_save(board)),
            SetEnum::MatrixGetRate(Item { value, .. }) => {
                self.matrix_get_rate.set(value);
                set.reply(Ok(()))
            }
            SetEnum::Refresh => set.reply(self.refresh()),
            SetEnum::BootLoaderUpdate(update) => set.reply(self.bootloader_update(update)),
            SetEnum::NoInput(board, no_input) => {
                set.reply(self.daemon.set_no_input(board, no_input))
            }
            SetEnum::Exit => return false,
        }

        true
    }

    fn matrix_refresh_all(&self) -> Result<(), String> {
        for (k, v) in self.boards.borrow_mut().iter_mut() {
            if !v.has_matrix {
                continue;
            }
            let matrix = self
                .daemon
                .matrix_get(*k)
                .map_err(|err| format!("failed to get matrix: {}", err))?;
            if v.matrix != matrix {
                let _ = v.matrix_channel.unbounded_send(matrix.clone());
                v.matrix = matrix;
            }
        }
        Ok(())
    }

    fn bootloader_update(&self, update: Option<Bootloaded>) -> Result<(), String> {
        *self.previous_bootloaded.borrow_mut() = *self.current_bootloaded.borrow();
        *self.current_bootloaded.borrow_mut() = update;
        let send = |msg| self.response_channel.unbounded_send(msg);

        // If a new board is plugged in and is in bootloader mode, update the gui
        // only check if we are in launch test mode for production.
        match (
            *self.previous_bootloaded.borrow(),
            *self.current_bootloaded.borrow(),
        ) {
            (None, Some(board)) => send(ThreadResponse::BootloadedAdded(board)),
            (Some(_), None) => send(ThreadResponse::BootloadedRemoved),
            _ => Ok(()),
        }
        .map_err(|err| format!("Failed to check for bootloader {}", err))
    }

    fn refresh(&self) -> Result<(), String> {
        self.daemon.refresh()?;

        let send = |msg| self.response_channel.unbounded_send(msg);
        let mut boards = self.boards.borrow_mut();

        let new_ids = self.daemon.boards()?;

        // Removed boards
        let response_channel = &self.response_channel;
        boards.retain(|id, _| {
            if !new_ids.iter().any(|i| i == id) {
                let _ = response_channel.unbounded_send(ThreadResponse::BoardRemoved(*id));
                return false;
            }
            true
        });

        // Added boards
        let mut have_new_board = false;
        for i in &new_ids {
            if boards.contains_key(i) {
                continue;
            }

            if !have_new_board {
                let _ = send(ThreadResponse::BoardLoading);
                have_new_board = true;
            }

            let (matrix_sender, matrix_reciever) = async_mpsc::unbounded();
            match Board::new(
                self.daemon.as_ref(),
                self.client.upgrade().unwrap(),
                *i,
                matrix_reciever,
            ) {
                Ok(board) => {
                    boards.insert(*i, ThreadBoard::new(matrix_sender, board.has_matrix()));
                    let _ = send(ThreadResponse::BoardAdded(board));
                }
                Err(err) => error!("Failed to add board: {}", err),
            }
        }

        if have_new_board {
            let _ = send(ThreadResponse::BoardLoadingDone);
        }

        Ok(())
    }
}
