//! Contains functions that initialize minus
//!
//! This module provides two main functions:-
//! * The [`init_core`] function which is responsible for setting the initial state of the
//! Pager, do enviroment checks and initializing various core functions on either async
//! tasks or native threads depending on the feature set
//!
//! * The [`start_reactor`] function displays the displays the output and also polls
//! the [`Receiver`] held inside the [`Pager`] for events. Whenever a event is
//! detected, it reacts to it accordingly.
use crate::{
    error::MinusError,
    input::InputEvent,
    minus_core::{
        ev_handler::handle_event,
        events::Event,
        utils::{display::draw_full, term},
        RunMode,
    },
    Pager, PagerState,
};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use crossterm::event;
#[cfg(feature = "static_output")]
use crossterm::tty::IsTty;
use std::{
    io::{stdout, Stdout},
    panic,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[cfg(feature = "static_output")]
use super::utils::display::write_lines;

#[cfg(feature = "search")]
use parking_lot::Condvar;
use parking_lot::Mutex;

use super::RUNMODE;

/// The main entry point of minus
///
/// This is called by both [`dynamic_paging`](crate::dynamic_paging) and
/// [`page_all`](crate::page_all) functions.
///
/// It first receives all events present inside the [`Pager`]'s receiver
/// and creates the initial state that to be stored inside the [`PagerState`]
///
/// Then it checks if the minus is running in static mode and does some checks:-
/// * If standard output is not a terminal screen, that is if it is a file or block
/// device, minus will write all the data at once to the stdout and quit
///
/// * If the size of the data is less than the available number of rows in the terminal
/// then it displays everything on the main stdout screen at once and quits. This
/// behaviour can be turned off if [`Pager::set_run_no_overflow(true)`] is called
/// by the main application
// Sorry... this behaviour would have been cool to have in async mode, just think about it!!! Many
// implementations were proposed but none were perfect
// It is because implementing this especially with line wrapping and terminal scrolling
// is a a nightmare because terminals are really naughty and more when you have to fight with it
// using your library... your only weapon
// So we just don't take any more proposals about this. It is really frustating to
// to throughly test each implementation and fix out all rough edges around it
/// Next it initializes the runtime and calls [`start_reactor`] and a [`event reader`]` which is
/// selected based on the enabled feature set:-
///
/// # Errors
///
/// Setting/cleaning up the terminal can fail and IO to/from the terminal can
/// fail.
///
/// [`event reader`]: event_reader
#[allow(clippy::module_name_repetitions)]
#[allow(clippy::too_many_lines)]
pub fn init_core(pager: &Pager, rm: RunMode) -> std::result::Result<(), MinusError> {
    #[allow(unused_mut)]
    let mut out = stdout();
    // Is the event reader running
    #[cfg(feature = "search")]
    let input_thread_running = Arc::new((Mutex::new(true), Condvar::new()));

    #[allow(unused_mut)]
    let mut ps = crate::state::PagerState::generate_initial_state(&pager.rx, &mut out)?;

    // Static mode checks
    #[cfg(feature = "static_output")]
    if *RUNMODE.lock() == RunMode::Static {
        // If stdout is not a tty, write everyhting and quit
        if !out.is_tty() {
            write_lines(&mut out, &[ps.lines], None)?;
            let mut rm = RUNMODE.lock();
            *rm = RunMode::Uninitialized;
            drop(rm);
            return Ok(());
        }
        // If number of lines of text is less than available wors, write everything and quit
        // unless run_no_overflow is set to true
        if ps.num_lines() <= ps.rows && !ps.run_no_overflow {
            write_lines(&mut out, &ps.formatted_lines, Some("\r"))?;
            ps.exit();
            let mut rm = RUNMODE.lock();
            *rm = RunMode::Uninitialized;
            drop(rm);
            return Ok(());
        }
    }

    {
        let mut runmode = super::RUNMODE.lock();
        assert!(runmode.is_uninitialized(), "Failed to set the RUNMODE. This is caused probably bcause another instance of minus is already running");
        *runmode = rm;
        drop(runmode);
    }

    // Setup terminal, adjust line wraps and get rows
    term::setup(&out)?;

    // Has the user quitted
    let is_exitted = Arc::new(AtomicBool::new(false));
    let is_exitted2 = is_exitted.clone();

    {
        let panic_hook = panic::take_hook();
        panic::set_hook(Box::new(move |pinfo| {
            is_exitted2.store(true, std::sync::atomic::Ordering::SeqCst);
            // While silently ignoring error is considered a bad practice, we are forced to do it here
            // as we cannot use the ? and panicking here will cause UB.
            drop(term::cleanup(
                stdout(),
                &crate::ExitStrategy::PagerQuit,
                true,
            ));
            panic_hook(pinfo);
        }));
    }

    let ps_mutex = Arc::new(Mutex::new(ps));

    let evtx = pager.tx.clone();
    let rx = pager.rx.clone();
    let out = stdout();

    let p1 = ps_mutex.clone();

    #[cfg(feature = "search")]
    let input_thread_running2 = input_thread_running.clone();

    std::thread::scope(|s| -> crate::Result {
        let out = Arc::new(out);
        let out_copy = out.clone();
        let is_exitted3 = is_exitted.clone();
        let is_exitted4 = is_exitted.clone();

        let t1 = s.spawn(move || {
            let res = event_reader(
                &evtx,
                &p1,
                #[cfg(feature = "search")]
                &input_thread_running2,
                &is_exitted3,
            );

            if res.is_err() {
                is_exitted3.store(true, std::sync::atomic::Ordering::SeqCst);
                let mut rm = RUNMODE.lock();
                *rm = RunMode::Uninitialized;
                drop(rm);
                term::cleanup(out.as_ref(), &crate::ExitStrategy::PagerQuit, true)?;
            }
            res
        });
        let t2 = s.spawn(move || {
            let res = start_reactor(
                &rx,
                &ps_mutex,
                &out_copy,
                #[cfg(feature = "search")]
                &input_thread_running,
                &is_exitted4,
            );

            if res.is_err() {
                is_exitted4.store(true, std::sync::atomic::Ordering::SeqCst);
                let mut rm = RUNMODE.lock();
                *rm = RunMode::Uninitialized;
                drop(rm);
                term::cleanup(out_copy.as_ref(), &crate::ExitStrategy::PagerQuit, true)?;
            }
            res
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        r1?;
        r2?;
        Ok(())
    })
}

/// Continously displays the output and reacts to events
///
/// This function displays the output continously while also checking for user inputs.
///
/// Whenever a event like a user input or instruction from the main application is detected
/// it will call [`handle_event`] to take required action for the event.
/// Then it will be do some checks if it is really necessory to redraw the screen
/// and redraw if it event requires it to do so.
///
/// For example if all rows in a terminal aren't filled and a
/// [`AppendData`](super::events::Event::AppendData) event occurs, it is absolutely necessory
/// to update the screen immidiately; while if all rows are filled, we can omit to redraw the
/// screen.
#[allow(clippy::too_many_lines)]
fn start_reactor(
    rx: &Receiver<Event>,
    ps: &Arc<Mutex<PagerState>>,
    out: &Stdout,
    #[cfg(feature = "search")] input_thread_running: &Arc<(Mutex<bool>, Condvar)>,
    is_exitted: &Arc<AtomicBool>,
) -> Result<(), MinusError> {
    let mut out_lock = out.lock();

    let mut p = ps.lock();
    draw_full(&mut out_lock, &mut p)?;
    drop(p);

    let run_mode = *RUNMODE.lock();
    match run_mode {
        #[cfg(feature = "dynamic_output")]
        RunMode::Dynamic => loop {
            if is_exitted.load(Ordering::SeqCst) {
                let mut rm = RUNMODE.lock();
                *rm = RunMode::Uninitialized;
                drop(rm);
                break;
            }

            let event = rx.recv();

            let mut p = ps.lock();

            match event {
                Ok(ev) if ev.required_immidiate_screen_update() => {
                    let is_exit_event = ev.is_exit_event();
                    let is_movement = ev.is_movement();
                    handle_event(
                        ev,
                        &mut out_lock,
                        &mut p,
                        is_exitted,
                        #[cfg(feature = "search")]
                        input_thread_running,
                    )?;
                    if !is_exit_event && !is_movement {
                        draw_full(&mut out_lock, &mut p)?;
                    }
                }
                // Ok(Event::UserInput(InputEvent::Search(search_mode)) => {
                //     if search_mode == Sear
                //
                // }
                Ok(ev) => {
                    handle_event(
                        ev,
                        &mut out_lock,
                        &mut p,
                        is_exitted,
                        #[cfg(feature = "search")]
                        input_thread_running,
                    )?;
                }
                Err(_) => {}
            }
        },
        #[cfg(feature = "static_output")]
        RunMode::Static => loop {
            if is_exitted.load(Ordering::SeqCst) {
                // Cleanup the screen
                //
                // This is not needed in dynamic paging because this is already handled by handle_event
                let p = ps.lock();
                term::cleanup(&mut out_lock, &p.exit_strategy, true)?;

                let mut rm = RUNMODE.lock();
                *rm = RunMode::Uninitialized;
                drop(rm);

                break;
            }

            if let Ok(Event::UserInput(inp)) = rx.recv() {
                let mut p = ps.lock();
                let is_exit_event = Event::UserInput(inp).is_exit_event();
                let is_movement = Event::UserInput(inp).is_movement();
                handle_event(
                    Event::UserInput(inp),
                    &mut out_lock,
                    &mut p,
                    is_exitted,
                    #[cfg(feature = "search")]
                    input_thread_running,
                )?;
                if !is_exit_event && !is_movement {
                    draw_full(&mut out_lock, &mut p)?;
                }
            }
        },
        RunMode::Uninitialized => panic!(
            "Static variable RUNMODE set to unitialized.\
This is most likely a bug. Please open an issue to the developers"
        ),
    }
    Ok(())
}

fn event_reader(
    evtx: &Sender<Event>,
    ps: &Arc<Mutex<PagerState>>,
    #[cfg(feature = "search")] user_input_active: &Arc<(Mutex<bool>, Condvar)>,
    is_exitted: &Arc<AtomicBool>,
) -> Result<(), MinusError> {
    loop {
        if is_exitted.load(Ordering::SeqCst) {
            break;
        }

        #[cfg(feature = "search")]
        {
            let (lock, cvar) = (&user_input_active.0, &user_input_active.1);
            let mut active = lock.lock();
            if !*active {
                cvar.wait(&mut active);
            }
        }

        if event::poll(std::time::Duration::from_millis(100))
            .map_err(|e| MinusError::HandleEvent(e.into()))?
        {
            let ev = event::read().map_err(|e| MinusError::HandleEvent(e.into()))?;
            let mut guard = ps.lock();
            // Get the events
            let input = guard.input_classifier.classify_input(ev, &guard);
            if let Some(iev) = input {
                if let InputEvent::Number(n) = iev {
                    guard.prefix_num.push(n);
                    guard.format_prompt();
                } else if !guard.prefix_num.is_empty() {
                    guard.prefix_num.clear();
                    guard.format_prompt();
                }
                if let Err(TrySendError::Disconnected(_)) = evtx.try_send(Event::UserInput(iev)) {
                    break;
                }
            } else if !guard.prefix_num.is_empty() {
                guard.prefix_num.clear();
                guard.format_prompt();
            }
        }
    }
    Result::<(), MinusError>::Ok(())
}
