extern crate mio;
extern crate bus;
extern crate time;
extern crate byteorder;

#[macro_use]
extern crate lazy_static;

use std::env;
use std::process::{exit};

use std::io::prelude::*;
use std::io::ErrorKind;
use mio::{Events, Poll, Ready, PollOpt, Token};

use mio::net::TcpStream;
use std::net::{TcpListener, SocketAddr};

use std::thread;
use std::time::Duration;
use std::sync::{Arc,Mutex};
use std::sync::mpsc;
use bus::Bus;

mod slippi;

const MAIN_THREAD_CYCLE: Duration = Duration::from_millis(10);

// Helper function for timing
fn timestamp() -> f64 {
    let timespec = time::get_time();
    let mills: f64 = timespec.sec as f64 + (timespec.nsec as f64 / 1000.0 / 1000.0 / 1000.0);
    mills
}

// Set up a global buffer for messages from console
lazy_static! {
    static ref GLOBAL_BUF: Mutex<Vec<Vec<u8>>> = Mutex::new(vec![vec![]]);
}

// Set up a global list of consumer threads 
lazy_static! {
    static ref THREAD_LIST: Mutex<Vec<usize>> = Mutex::new(vec![]);
}

// Channel from console thread to N consumer threads
lazy_static! {
    static ref BUS: Arc<Mutex<Bus<u8>>> = Arc::new(Mutex::new(Bus::new(32)));
}



fn main() {

    // Handle command-line arguments from the user
    let args: Vec<String> = env::args().collect();
    let host: String = if args.len() >= 2 {
        String::from(format!("{}:{}", &args[1], 666))
    } else { 
        println!("usage: andross <console IP address>"); 
        exit(-1); 
    };

    // Set up a poll handle and container for polling events
    let poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(1024);

    // Consumer thread ID, used later
    let mut tid = 0;

    // Channel from N consumer threads to the console thread
    let (m_tx, m_rx) = mpsc::sync_channel(0);

    // Connect to the console stream
    let addr: SocketAddr = host.parse().unwrap();
    let mut console_stream = match TcpStream::connect(&addr) {
        Ok(console_stream) => console_stream,
        Err(y) => {
            panic!("[main]\t{}", y);
        },
    };

    // Register the console stream with poll
    poll.register(&console_stream, Token(0), Ready::all(), PollOpt::edge()).unwrap();

     // Check if we connected, otherwise die.
    poll.poll(&mut events, None).unwrap();
    for event in &events {
        if event.token() == Token(0) && event.readiness().is_hup() {
            println!("[console]\tCouldn't connect to the console!");
            console_stream.shutdown(mio::tcp::Shutdown::Both).unwrap();
            std::process::exit(-1);
        }
    }


    // Closure for the console thread
    let mut total_msgcount = 0;
    thread::spawn(move || {

        println!("[console]\tStarted console socket!");

        'console_loop: loop {
            poll.poll(&mut events, None).unwrap();
            for event in &events {

                // If the socket is readable
                if event.token() == Token(0) && event.readiness().is_readable() {

                    // If the client hung up, break out of this loop
                    if event.readiness().is_hup() {
                        console_stream.shutdown(mio::tcp::Shutdown::Both).unwrap();
                        println!("[console]\tThe console hung up our connection.");
                        break 'console_loop;
                    }

                    // Read the message into a vector
                    let mut message = vec![];
                    match console_stream.read_to_end(&mut message) {
                        Ok(_)   => {},
                        Err(y)  => {
                            if y.kind() != ErrorKind::WouldBlock {
                                panic!("[console]\tI/O error ({})", y);
                            }
                        },
                    };
                    total_msgcount += 1;

                    // Parse Slippi commands within the message
                    let msg = slippi::parse_message(&message);

                    // Push a new message onto the global buffer
                    GLOBAL_BUF.lock().unwrap().push(message);

                    // Emit a channel message to all consumer threads
                    BUS.lock().unwrap().broadcast(msg);
                    println!("[console]\t{:?} emit {}", timestamp(), msg);

                    match msg {
                        slippi::GAME_END   => {
                            println!("[console] Going to clear buffer, waiting...");

                            // Acquire lock, get current list of threads
                            let mut consumers = THREAD_LIST.lock().unwrap().to_vec();

                            // Block until all consumers are accounted for
                            while consumers.len() != 0 {
                                println!("[console]\tWaiting for {:?}", consumers);
                                let tid = m_rx.recv().unwrap();
                                consumers.retain(|x| x != &tid);
                            }

                            // Flush to disk, or something
                            // <impl here...>

                            // Free up messages from this session
                            GLOBAL_BUF.lock().unwrap().clear();
                            println!("[console]\tFlushed memory");
                        },
                        _       => {},
                    };
                } 
            }
        }
    });


    // Bind to localhost and spawn a new thread for each client
    let listener = TcpListener::bind("127.0.0.1:666").unwrap();
    for s in listener.incoming() {

        // Increment the thread ID, then add it to the list
        tid += 1; THREAD_LIST.lock().unwrap().push(tid);

        // Set up channels for the new thread
        let mut rx = BUS.lock().unwrap().add_rx();
        let tx = m_tx.clone();

        // Unwrap/setup the stream managed by the new thread
        let mut stream = TcpStream::from_stream(s.unwrap()).unwrap();
        stream.set_nodelay(true).unwrap();

        // This is the closure for consumer threads
        thread::spawn(move || {
            let mut read_cur = 0;
            let threadname = String::from(format!("consumer-{}", tid));
            println!("[{}] Thread spawned for consumer", threadname);

            'consumer_loop: loop {

                // We wait off-CPU here until we get a channel message
                let state = rx.recv().unwrap();
                println!("[{}]\t{:?} got {}", threadname, timestamp(), state);

                // Block until we acquire the lock and unwrap the buffer
                let buffer = GLOBAL_BUF.lock().unwrap();
                let write_cur = buffer.len();

                // Read and send() until we catch up to the write cursor
                while read_cur < write_cur {
                    match stream.write(buffer.get(read_cur).unwrap()) {
                        Ok(_) => {},
                        Err(y) => {
                            println!("[{}]\tDisconnected ({})", threadname, y);
                            THREAD_LIST.lock().unwrap().retain(|x| x != &tid);
                            break 'consumer_loop;
                        },
                    };
                    read_cur += 1;
                }
                println!("[{}]\t{:?} cursor synced", threadname, timestamp());

                match state {
                    slippi::GAME_END    => {
                        tx.send(tid).unwrap();
                        read_cur = 0;
                        println!("[{}]\tReset local cursor", threadname);
                    },
                    _           => {},
                };
            }
        });
    }

    // Let the main thread just wait around, for now
    loop { thread::sleep(MAIN_THREAD_CYCLE); }
}
