use crate::{BoardEvent, Conf, Counter, EventWrapper, FELibReturn, HDF5Writer};
use anyhow::Result;
use crossbeam_channel::{tick, unbounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Stylize,
    symbols::border,
    text::{Line, Text},
    widgets::{Block, Paragraph, Widget},
    DefaultTerminal, Frame,
};
use std::{
    fs::DirEntry,
    path::PathBuf,
    time::{Duration, Instant},
};
use std::{sync::atomic::Ordering, thread::JoinHandle};
use std::{
    sync::{atomic::AtomicBool, Arc, Condvar, Mutex},
    thread,
};

#[derive(Default, Clone, Copy)]
struct RunInfo {
    board0_info: BoardInfo,
    board1_info: BoardInfo,
    event_channel_buf: usize,
}

impl RunInfo {
    fn event_size(&self) -> usize {
        self.board0_info.event_size + self.board1_info.event_size
    }
}

#[derive(Default, Clone, Copy)]
struct BoardInfo {
    event_size: usize,
    trigger_id: u32,
    board_id: usize,
}

#[derive(Debug)]
pub struct Status {
    pub counter: Counter,
    pub t_begin: Instant,
    pub run_duration: Duration,
    pub run_num: usize,
    pub camp_num: usize,
    pub curr_run: usize,
    pub buffer_len: usize,
    pub config: Conf,
    pub boards: Vec<(usize, u64)>,
    pub max_runs: Option<usize>,
    pub exit: Option<StatusExit>,
}

#[derive(Debug, Clone, Copy)]
pub enum StatusExit {
    Quit,
    Timeout,
}

impl Status {
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let ticker = tick(Duration::from_secs(1));
        let max_runs = self.max_runs.unwrap_or(0);

        loop {
            self.t_begin = Instant::now();
            self.exit = None;
            self.counter.reset();
            self.buffer_len = 0;

            let shutdown = Arc::new(AtomicBool::new(false));
            let (tx_stats, rx_stats) = unbounded();
            let (tx_events, ev_handle, data_taking_handle) =
                self.begin_run(Arc::clone(&shutdown), tx_stats)?;

            while self.exit.is_none() {
                let _ = ticker.recv();

                // Drain stats channel
                match rx_stats.try_recv() {
                    Ok(run_info) => {
                        self.counter.increment(run_info.event_size());
                        self.buffer_len = run_info.event_channel_buf;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        self.exit = Some(StatusExit::Quit);
                    }
                }
                while let Ok(run_info) = rx_stats.try_recv() {
                    self.counter.increment(run_info.event_size());
                    self.buffer_len = run_info.event_channel_buf;
                }

                self.handle_events()?;

                if self.t_begin.elapsed() >= self.run_duration {
                    self.exit = Some(StatusExit::Timeout);
                }

                terminal.draw(|f| self.draw(f))?;
            }

            // If user quit, record that so outer loop can break
            if let Some(StatusExit::Quit) = self.exit {
                shutdown.store(true, Ordering::SeqCst);
            }

            // disarm boards
            for &(_, dev) in &self.boards {
                crate::felib_sendcommand(dev, "/cmd/disarmacquisition")?;
            }
            // join board threads
            data_taking_handle
                .join()
                .expect("data taking thread panic")?;
            // drop tx_events so event thread will exit
            drop(tx_events);
            // wait for event‐processing to finish
            ev_handle.join().expect("event thread panic")?;

            // if user quit, break out of the outer loop
            if let Some(StatusExit::Quit) = self.exit {
                // Close all boards
                for &(_, dev_handle) in &self.boards {
                    crate::felib_close(dev_handle)?;
                }
                return Ok(());
            }
            self.curr_run += 1;
            if self.curr_run == max_runs && max_runs != 0 {
                // Close all boards
                for &(_, dev_handle) in &self.boards {
                    crate::felib_close(dev_handle)?;
                }
                return Ok(());
            }
            // else (Timeout) start next run automatically
        }
    }

    pub fn new(config: Conf, boards: Vec<(usize, u64)>, max_runs: Option<usize>) -> Self {
        let run_duration = Duration::from_secs(config.run_settings.run_duration);
        let camp_num = config.run_settings.campaign_num;
        Self {
            counter: Counter::default(),
            t_begin: Instant::now(),
            run_duration,
            run_num: 0,
            camp_num,
            curr_run: 0,
            config,
            boards,
            max_runs,
            exit: None,
            buffer_len: 0,
        }
    }

    fn draw(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> Result<()> {
        if event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                    self.handle_key_event(key_event)
                }
                _ => {}
            };
        }
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('q') => self.exit(),
            _ => {}
        }
    }

    fn exit(&mut self) {
        self.exit = Some(StatusExit::Quit);
    }

    fn begin_run(
        &mut self,
        shutdown: Arc<AtomicBool>,
        tx_stats: Sender<RunInfo>,
    ) -> Result<(
        Sender<BoardEvent>,
        JoinHandle<Result<()>>,
        JoinHandle<Result<()>>,
    )> {
        // Shared signal for acquisition start.
        let acq_start = Arc::new((Mutex::new(false), Condvar::new()));
        // Shared counter for endpoint configuration.
        let endpoint_configured = Arc::new((Mutex::new(false), Condvar::new()));

        // Channel to receive events from board threads.
        let (tx_events, rx_events) = unbounded();

        // Spawn a data-taking thread .
        let boards_clone = self.boards.clone();
        let config_clone = self.config.clone();
        let acq_start_clone = Arc::clone(&acq_start);
        let endpoint_configured_clone = Arc::clone(&endpoint_configured);
        let tx_clone = tx_events.clone();
        let shutdown_clone = Arc::clone(&shutdown);
        let data_taking_handle = thread::spawn(move || {
            data_taking_thread(
                boards_clone,
                config_clone,
                tx_clone,
                acq_start_clone,
                endpoint_configured_clone,
                shutdown_clone,
            )
        });

        // Wait until all boards have configured their endpoints.
        {
            let (lock, cond) = &*endpoint_configured;
            let mut configured = lock.lock().unwrap();
            while !*configured {
                configured = cond.wait(configured).unwrap();
            }
        }

        // Signal acquisition start.
        {
            let (lock, cvar) = &*acq_start;
            let mut started = lock.lock().unwrap();
            *started = true;
            cvar.notify_all();
        }

        // Begin run acquisition.
        crate::felib_sendcommand(self.boards[0].1, "/cmd/swstartacquisition")?;

        // Create the appropriate directory for file-writing
        let run_file = self.create_run_file().unwrap();

        // Spawn a dedicated thread to process incoming events and print global stats.
        let config_clone = self.config.clone();
        let shutdown_clone = Arc::clone(&shutdown);
        let event_processing_handle = thread::spawn(move || -> Result<()> {
            event_processing(rx_events, tx_stats, run_file, config_clone, shutdown_clone)
        });

        Ok((tx_events, event_processing_handle, data_taking_handle))
    }

    fn create_run_file(&mut self) -> Result<PathBuf> {
        let mut camp_dir = self.create_camp_dir().unwrap();
        let runs: Vec<DirEntry> = std::fs::read_dir(&camp_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let max_run = runs
            .iter()
            .filter_map(|path| {
                path.file_name()
                    .to_str() // Get file name (OsStr)
                    .and_then(|filename| {
                        // Ensure the filename starts with "run"
                        if let Some(stripped) = filename.strip_prefix("run") {
                            // Split at '_' and take the first part
                            let parts: Vec<&str> = stripped.split('_').collect();
                            parts.first()?.parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
            })
            .max();

        if let Some(max) = max_run {
            let file = format!("run{}_0.h5", max + 1);
            camp_dir.push(&file);
            self.run_num = max + 1;
            Ok(camp_dir)
        } else {
            Ok(camp_dir.join("run0_0.h5"))
        }
    }

    fn create_camp_dir(&self) -> Result<PathBuf> {
        let camp_dir = format!(
            "{}/camp{}",
            self.config.run_settings.output_dir, self.config.run_settings.campaign_num
        );
        let path = PathBuf::from(camp_dir);
        if !std::fs::exists(&path).unwrap() {
            match std::fs::create_dir_all(&path) {
                Ok(_) => {
                    println!("Create campaign directory");
                }
                Err(e) => {
                    eprintln!("Error creating dir: {:?}", e)
                }
            }
        }

        Ok(path)
    }
}

impl Widget for &Status {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title =
            Line::from(format!(" Campaign {} Run {} Status ", self.camp_num, self.run_num).bold());
        let instructrions = Line::from(vec![" Quit ".into(), "<Q> ".blue().bold()]);
        let block = Block::bordered()
            .title(title.centered())
            .title_bottom(instructrions.centered())
            .border_set(border::THICK);

        let status_text = Text::from(vec![Line::from(vec![
            "Elapsed time: ".into(),
            self.counter
                .t_begin
                .elapsed()
                .as_secs()
                .to_string()
                .yellow(),
            " s".into(),
            " Events: ".into(),
            self.counter.n_events.to_string().yellow(),
            " Data rate: ".into(),
            format!("{:.2}", self.counter.rate()).yellow(),
            " MB/s ".into(),
            " Buffer length: ".into(),
            self.buffer_len.to_string().yellow(),
        ])]);

        Paragraph::new(status_text)
            .centered()
            .block(block)
            .render(area, buf);
    }
}

fn event_processing(
    rx: Receiver<BoardEvent>,
    tx_stats: Sender<RunInfo>,
    run_file: PathBuf,
    config: Conf,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut writer =
        HDF5Writer::new(run_file, 64, config.board_settings.record_len, 7500, 50).unwrap();
    let mut run_info = RunInfo::default();
    let mut events = Vec::with_capacity(4);
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(mut board_event) => {
                match board_event.board_id {
                    0 => {
                        run_info.board0_info.event_size = board_event.event.c_event.event_size;
                        run_info.board0_info.board_id = 0;
                        run_info.board0_info.trigger_id = board_event.event.c_event.trigger_id;
                        run_info.event_channel_buf += rx.len();
                    }
                    1 => {
                        run_info.board1_info.event_size = board_event.event.c_event.event_size;
                        run_info.board1_info.board_id = 1;
                        run_info.board1_info.trigger_id = board_event.event.c_event.trigger_id;
                        run_info.event_channel_buf += rx.len();
                    }
                    _ => unreachable!(),
                }
                zero_suppress(&mut board_event);
                events.push(board_event);
                if events.len() == 2 {
                    if tx_stats.send(run_info).is_err() {
                        break;
                    }
                    if run_info.board0_info.trigger_id != run_info.board1_info.trigger_id {
                        break;
                    }
                    writer
                        .append_event(
                            events[0].board_id,
                            events[0].event.c_event.timestamp,
                            &events[0].event.waveform_data,
                        )
                        .unwrap();
                    writer
                        .append_event(
                            events[1].board_id,
                            events[1].event.c_event.timestamp,
                            &events[1].event.waveform_data,
                        )
                        .unwrap();
                    run_info = RunInfo::default();
                    events.clear();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // If no event is received within the timeout, check if it's time to print.
            }
            Err(RecvTimeoutError::Disconnected) => {
                writer.flush_all().unwrap();
                break;
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            writer.flush_all().unwrap();
            break;
        }
    }

    drop(tx_stats);
    Ok(())
}

/// Data-taking thread function for one board.
/// It configures the endpoint, signals that configuration is complete,
/// waits for the shared acquisition start signal, then continuously reads events and sends them.
fn data_taking_thread(
    boards: Vec<(usize, u64)>,
    config: Conf,
    tx: Sender<BoardEvent>,
    acq_start: Arc<(Mutex<bool>, Condvar)>,
    endpoint_configured: Arc<(Mutex<bool>, Condvar)>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    // Set up endpoint.
    let mut ep_handles = vec![(0, 0); boards.len()];
    for (&(_, dev_handle), (ep_handle, ep_folder_handle)) in
        boards.iter().zip(ep_handles.iter_mut())
    {
        crate::felib_gethandle(dev_handle, "/endpoint/scope", ep_handle)?;
        crate::felib_getparenthandle(*ep_handle, "", ep_folder_handle)?;
        crate::felib_setvalue(*ep_folder_handle, "/par/activeendpoint", "scope")?;
        crate::felib_setreaddataformat(*ep_handle, crate::EVENT_FORMAT)?;
        crate::felib_sendcommand(dev_handle, "/cmd/armacquisition")?;
    }

    // Signal that this board's endpoint is configured.
    {
        let (lock, cond) = &*endpoint_configured;
        let mut configured = lock.lock().unwrap();
        *configured = true;
        cond.notify_all();
    }

    // Wait for the acquisition start signal.
    {
        let (lock, cvar) = &*acq_start;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }
    }

    // Data-taking loop.
    // num_ch has to be 64 due to the way CAEN reads data from the board
    let num_ch = 64;
    let waveform_len = config.board_settings.record_len;
    let mut events = vec![EventWrapper::new(num_ch, waveform_len); boards.len()];
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let mut board_events = Vec::new();
        let mut successes = 0;
        for &(board_id, dev_handle) in &boards {
            let ret = crate::felib_readdata(dev_handle, &mut events[board_id]);
            match ret {
                FELibReturn::Success => {
                    // Instead of allocating a new EventWrapper,
                    // swap out the current one using std::mem::replace.
                    let board_event = BoardEvent {
                        board_id,
                        event: std::mem::replace(
                            &mut events[board_id],
                            EventWrapper::new(num_ch, waveform_len),
                        ),
                    };
                    board_events.push(board_event);
                    successes += 1;
                }
                FELibReturn::Timeout => continue,
                FELibReturn::Stop => {
                    // println!("Board {}: Stop received...", board_id);
                    break;
                }
                _ => (),
            }
        }
        if successes == 2 {
            for board_event in board_events {
                if tx.send(board_event).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn zero_suppress(board_data: &mut BoardEvent) {
    board_data
        .event
        .waveform_data
        .par_map_inplace(|adc| *adc = 0);
}
