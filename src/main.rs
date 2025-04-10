use anyhow::Result;
use clap::Parser;
use confique::Config;
use core::str;
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
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
use rust_daq::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const EVENT_FORMAT: &str = " \
    [ \
        { \"name\" : \"TIMESTAMP_NS\", \"type\" : \"U64\" }, \
        { \"name\" : \"TRIGGER_ID\", \"type\" : \"U32\" }, \
        { \"name\" : \"WAVEFORM\", \"type\" : \"U16\", \"dim\" : 2 }, \
        { \"name\" : \"WAVEFORM_SIZE\", \"type\" : \"SIZE_T\", \"dim\" : 1 }, \
        { \"name\" : \"EVENT_SIZE\", \"type\" : \"SIZE_T\" } \
    ] \
";

/// LAr DAQ program
#[derive(Parser, Debug)]
struct Args {
    /// Config file used for data acquisition
    #[arg(long, short)]
    pub config: String,
    /// Optional number of runs if indefinite isn't desired
    runs: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = Conf::from_file(args.config).map_err(|_| FELibReturn::InvalidParam)?;

    // List of board connection strings. Add as many as needed.
    let board_urls = vec!["dig2://caendgtz-usb-25380", "dig2://caendgtz-usb-25379"];

    // Open boards and store their handles along with an assigned board ID.
    let mut boards = Vec::new();
    for (i, url) in board_urls.iter().enumerate() {
        let dev_handle = felib_open(url)?;
        println!("\nBoard {} details:", i);
        print_dig_details(dev_handle)?;
        boards.push((i, dev_handle));
    }

    // Reset all boards.
    print!("\nResetting boards...\t");
    for &(_, dev_handle) in &boards {
        felib_sendcommand(dev_handle, "/cmd/reset")?;
    }
    println!("done.");

    // Configure all boards.
    print!("Configuring boards...\t");
    for &(_, dev_handle) in &boards {
        configure_board(dev_handle, &config)?;
    }
    println!("done.");

    // Configure sync settings
    print!("Configuring sync...\t");
    for &(i, dev_handle) in &boards {
        configure_sync(dev_handle, i as isize, board_urls.len() as isize, &config)?;
    }
    println!("done.");

    let mut curr_run = 0;
    let shutdown = Arc::new(AtomicBool::new(false));
    while !shutdown.load(Ordering::SeqCst) {
        curr_run += 1;
        let shutdown_clone = Arc::clone(&shutdown);
        let (tx, event_processing_handle, board_threads) =
            begin_run(&config, &boards, shutdown_clone)?;

        let reason = event_processing_handle
            .join()
            .expect("event processing panicked")?;

        // Stop acquisition on all boards.
        for &(_, dev_handle) in &boards {
            felib_sendcommand(dev_handle, "/cmd/disarmacquisition")?;
        }
        for handle in board_threads {
            handle.join().expect("A board thread panicked");
        }
        // Close the tx channel so that the event processing thread can exit.
        drop(tx);

        match reason {
            StatusExit::Quit => {
                break;
            }
            StatusExit::Timeout => {
                if let Some(max) = args.runs {
                    if curr_run >= max {
                        break;
                    }
                }
                continue;
            }
        }
    }

    // Close all boards.
    for &(_, dev_handle) in &boards {
        felib_close(dev_handle)?;
    }

    println!("\nTTFN!");

    Ok(())
}

/// Prints details for a given board.
fn print_dig_details(handle: u64) -> Result<(), FELibReturn> {
    let model = felib_getvalue(handle, "/par/ModelName")?;
    println!("Model name:\t{model}");
    let serialnum = felib_getvalue(handle, "/par/SerialNum")?;
    println!("Serial number:\t{serialnum}");
    let adc_nbit = felib_getvalue(handle, "/par/ADC_Nbit")?;
    println!("ADC bits:\t{adc_nbit}");
    let numch = felib_getvalue(handle, "/par/NumCh")?;
    println!("Channels:\t{numch}");
    let samplerate = felib_getvalue(handle, "/par/ADC_SamplRate")?;
    println!("ADC rate:\t{samplerate}");
    let cupver = felib_getvalue(handle, "/par/cupver")?;
    println!("CUP version:\t{cupver}");
    Ok(())
}

/// Data-taking thread function for one board.
/// It configures the endpoint, signals that configuration is complete,
/// waits for the shared acquisition start signal, then continuously reads events and sends them.
fn data_taking_thread(
    board_id: usize,
    dev_handle: u64,
    config: Conf,
    tx: Sender<BoardEvent>,
    acq_start: Arc<(Mutex<bool>, Condvar)>,
    endpoint_configured: Arc<(Mutex<u32>, Condvar)>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), FELibReturn> {
    // Set up endpoint.
    let mut ep_handle = 0;
    let mut ep_folder_handle = 0;
    felib_gethandle(dev_handle, "/endpoint/scope", &mut ep_handle)?;
    felib_getparenthandle(ep_handle, "", &mut ep_folder_handle)?;
    felib_setvalue(ep_folder_handle, "/par/activeendpoint", "scope")?;
    felib_setreaddataformat(ep_handle, EVENT_FORMAT)?;
    felib_sendcommand(dev_handle, "/cmd/armacquisition")?;

    // Signal that this board's endpoint is configured.
    {
        let (lock, cond) = &*endpoint_configured;
        let mut count = lock.lock().unwrap();
        *count += 1;
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
    let mut event = EventWrapper::new(num_ch, waveform_len);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let ret = felib_readdata(ep_handle, &mut event);
        match ret {
            FELibReturn::Success => {
                // Instead of allocating a new EventWrapper,
                // swap out the current one using std::mem::replace.
                let board_event = BoardEvent {
                    board_id,
                    event: std::mem::replace(&mut event, EventWrapper::new(num_ch, waveform_len)),
                };
                if tx.send(board_event).is_err() {
                    break;
                }
            }
            FELibReturn::Timeout => continue,
            FELibReturn::Stop => {
                print_status(
                    &format!("Board {}: Stop received...\n", board_id),
                    false,
                    true,
                    false,
                );
                break;
            }
            _ => (),
        }
    }
    Ok(())
}

fn get_clock_out_delay(board_id: isize, num_boards: isize) -> isize {
    let first_board = board_id == 0;
    let last_board = board_id == num_boards - 1;

    if last_board {
        0
    } else if first_board {
        -2148
    } else {
        -3111
    }
}

fn get_run_delay(board_id: isize, num_boards: isize) -> isize {
    let first_board = board_id == 0;
    let board_id_from_last = num_boards - board_id - 1;

    let mut run_delay_clk = 2 * board_id_from_last;

    if first_board {
        run_delay_clk += 4;
    }

    run_delay_clk * 8
}

fn configure_board(handle: u64, config: &Conf) -> Result<(), FELibReturn> {
    match config.board_settings.en_chans {
        ChannelConfig::All(_) => {
            felib_setvalue(handle, "/ch/0..63/par/ChEnable", "true")?;
        }
        ChannelConfig::List(ref channels) => {
            for channel in channels {
                let path = format!("/ch/{}/par/ChEnable", channel);
                felib_setvalue(handle, &path, "true")?;
            }
        }
    }
    match config.board_settings.dc_offset {
        DCOffsetConfig::Global(offset) => {
            felib_setvalue(handle, "/ch/0..63/par/DCOffset", &offset.to_string())?;
        }
        DCOffsetConfig::PerChannel(ref map) => {
            for (chan, offset) in map {
                let path = format!("/ch/{}/par/DCOffset", chan);

                felib_setvalue(handle, &path, &offset.to_string())?;
            }
        }
    }
    felib_setvalue(
        handle,
        "/par/RecordLengthS",
        &config.board_settings.record_len.to_string(),
    )?;
    felib_setvalue(
        handle,
        "/par/PreTriggerS",
        &config.board_settings.pre_trig_len.to_string(),
    )?;
    felib_setvalue(
        handle,
        "/par/AcqTriggerSource",
        &config.board_settings.trig_source,
    )?;
    felib_setvalue(handle, "/par/TestPulsePeriod", "8333333")?;
    felib_setvalue(handle, "/par/TestPulseWidth", "1000")?;
    felib_setvalue(handle, "/par/TestPulseLowLevel", "0")?;
    felib_setvalue(handle, "/par/TestPulseHighLevel", "10000")?;

    Ok(())
}

fn configure_sync(
    handle: u64,
    board_id: isize,
    num_boards: isize,
    config: &Conf,
) -> Result<(), FELibReturn> {
    let first_board = board_id == 0;

    felib_setvalue(
        handle,
        "/par/ClockSource",
        if first_board {
            &config.sync_settings.primary_clock_src
        } else {
            &config.sync_settings.secondary_clock_src
        },
    )?;
    felib_setvalue(
        handle,
        "/par/SyncOutMode",
        if first_board {
            &config.sync_settings.primary_sync_out
        } else {
            &config.sync_settings.secondary_sync_out
        },
    )?;
    felib_setvalue(
        handle,
        "/par/StartSource",
        if first_board {
            &config.sync_settings.primary_start_source
        } else {
            &config.sync_settings.secondary_start_source
        },
    )?;
    felib_setvalue(
        handle,
        "/par/EnClockOutFP",
        if first_board {
            &config.sync_settings.primary_clock_out_fp
        } else {
            &config.sync_settings.secondary_clock_out_fp
        },
    )?;
    felib_setvalue(
        handle,
        "/par/EnAutoDisarmAcq",
        &config.sync_settings.auto_disarm,
    )?;
    felib_setvalue(handle, "/par/TrgOutMode", &config.sync_settings.trig_out)?;

    let run_delay = get_run_delay(board_id, num_boards);
    let clock_out_delay = get_clock_out_delay(board_id, num_boards);
    felib_setvalue(handle, "/par/RunDelay", &run_delay.to_string())?;
    felib_setvalue(
        handle,
        "/par/VolatileClockOutDelay",
        &clock_out_delay.to_string(),
    )?;

    Ok(())
}

#[derive(Debug)]
struct Status {
    counter: Counter,
    rx: Receiver<(usize, usize)>,
    t_begin: Instant,
    run_duration: Duration,
    run_num: usize,
    camp_num: usize,
    buffer_len: usize,
    exit: Option<StatusExit>,
}

#[derive(Debug, Clone, Copy)]
enum StatusExit {
    Quit,
    Timeout,
}

impl Status {
    pub fn run(
        &mut self,
        terminal: &mut Arc<Mutex<DefaultTerminal>>,
        shutdown: Arc<AtomicBool>,
        rx: Receiver<()>,
    ) -> Result<StatusExit> {
        while self.exit.is_none() {
            if rx.recv().is_err() {
                break;
            }
            while let Ok(size) = self.rx.try_recv() {
                self.counter.increment(size.0);
                self.buffer_len = size.1;
            }
            self.handle_events()?;
            if self.t_begin.elapsed() > self.run_duration {
                self.exit = Some(StatusExit::Timeout);
            }
            terminal.lock().unwrap().draw(|frame| self.draw(frame))?;
        }
        if let Some(StatusExit::Quit) = self.exit {
            shutdown.store(true, Ordering::SeqCst);
        }
        let exit_status = self.exit.unwrap();
        Ok(exit_status)
    }

    pub fn new(
        rx: Receiver<(usize, usize)>,
        run_duration: Duration,
        camp_num: usize,
        run_num: usize,
    ) -> Self {
        Self {
            counter: Counter::default(),
            rx,
            t_begin: Instant::now(),
            run_duration,
            run_num,
            camp_num,
            exit: None,
            buffer_len: 0,
        }
    }

    fn draw(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }

    fn handle_events(&mut self) -> Result<()> {
        if event::poll(Duration::from_millis(10))? {
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
    run_file: PathBuf,
    run_num: usize,
    config: Conf,
    boards: Vec<(usize, u64)>,
    shutdown: Arc<AtomicBool>,
) -> Result<StatusExit> {
    let (tick_tx, tick_rx) = unbounded();
    let tick_rate = Duration::from_secs(1);
    // Spawn the ticker thread
    thread::spawn(move || {
        while tick_tx.send(()).is_ok() {
            thread::sleep(tick_rate);
        }
    });

    let shutdown_clone = Arc::clone(&shutdown);
    let run_duration = Duration::from_secs(config.run_settings.run_duration);
    let (tx_user, rx_user) = unbounded();
    let terminal = ratatui::init();
    // Spawn the TUI in a background thread
    let tui_handle = {
        let mut terminal = Arc::new(Mutex::new(terminal));
        std::thread::spawn(move || {
            let mut status = Status::new(
                rx_user,
                run_duration,
                config.run_settings.campaign_num,
                run_num,
            );
            // This will now run in parallel
            status.run(&mut terminal, shutdown_clone, tick_rx).unwrap()
        })
    };

    let board_handles: Vec<u64> = boards.iter().map(|(_, h)| *h).collect();
    let mut prev_len = 0;
    let mut writer =
        HDF5Writer::new(run_file, 64, config.board_settings.record_len, 7500, 50).unwrap();
    loop {
        // Use a blocking recv with timeout to periodically print stats.
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(board_event) => {
                // stats.increment(board_event.event.c_event.event_size);
                match tx_user.send((board_event.event.c_event.event_size, rx.len())) {
                    Ok(_) => (),
                    Err(_) => break,
                }
                // You can also log which board the event came from if needed.
                writer
                    .append_event(
                        board_event.board_id,
                        board_event.event.c_event.timestamp,
                        &board_event.event.waveform_data,
                    )
                    .unwrap();
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
            break;
        }
    }

    drop(tx_user);
    ratatui::restore();
    let reason = tui_handle.join().expect("TUI panicked");
    Ok(reason)
}

fn begin_run(
    config: &Conf,
    boards: &Vec<(usize, u64)>,
    shutdown: Arc<AtomicBool>,
) -> Result<(
    Sender<BoardEvent>,
    JoinHandle<Result<StatusExit>>,
    Vec<JoinHandle<()>>,
)> {
    print_status("Beginning new run", true, true, false);
    // Shared signal for acquisition start.
    let acq_start = Arc::new((Mutex::new(false), Condvar::new()));
    // Shared counter for endpoint configuration.
    let endpoint_configured = Arc::new((Mutex::new(0u32), Condvar::new()));

    // Channel to receive events from board threads.
    let (tx, rx) = unbounded();

    // Spawn a data-taking thread for each board.
    let mut board_threads = Vec::new();
    for &(board_id, dev_handle) in boards {
        let config_clone = config.clone();
        let acq_start_clone = Arc::clone(&acq_start);
        let endpoint_configured_clone = Arc::clone(&endpoint_configured);
        let tx_clone = tx.clone();
        let shutdown_clone = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            data_taking_thread(
                board_id,
                dev_handle,
                config_clone,
                tx_clone,
                acq_start_clone,
                endpoint_configured_clone,
                shutdown_clone,
            )
            .unwrap_or_else(|e| eprintln!("Board {} error: {:?}", board_id, e));
        });
        board_threads.push(handle);
    }

    // Wait until all boards have configured their endpoints.
    {
        let (lock, cond) = &*endpoint_configured;
        let mut count = lock.lock().unwrap();
        while *count < boards.len() as u32 {
            count = cond.wait(count).unwrap();
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
    print_status(
        "Starting acquisition on primary board...",
        false,
        true,
        false,
    );
    felib_sendcommand(boards[0].1, "/cmd/swstartacquisition")?;
    print_status("done.", false, false, false);

    // Create the appropriate directory for file-writing
    let (run_file, run_num) = create_run_file(config).unwrap();

    // Spawn a dedicated thread to process incoming events and print global stats.
    let config_clone = config.clone();
    let boards_clone = boards.clone();
    let shutdown_clone = Arc::clone(&shutdown);
    let event_processing_handle = thread::spawn(move || -> Result<StatusExit> {
        event_processing(
            rx,
            run_file,
            run_num,
            config_clone,
            boards_clone,
            shutdown_clone,
        )
    });

    Ok((tx, event_processing_handle, board_threads))
}
