use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::usize;
use std::{fs, u32};

use async_channel::{bounded, Receiver, Sender};
use bytes::Bytes;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};

use crate::errors::ProtocolError;

const FLAG_U8_SIZE: usize = 2;
const NUM_U8_SIZE: usize = 2;
const TASK_ID_U8_SIZE: usize = 4;
const LENGTH_U8_SIZE: usize = 4;

const BIT_STATUS_OK: u16 = 0b1;
const BIT_STATUS_BAD_REQ: u16 = 0b10;
const BIT_STATUS_VALIDATION_ERR: u16 = 0b100;
const BIT_STATUS_INTERNAL_ERR: u16 = 0b1000;

#[derive(Debug, Clone, Copy)]
pub enum TaskCode {
    UnknownError,
    Normal,
    BadRequestError,
    ValidationError,
    InternalError,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub code: TaskCode,
    pub data: Bytes,
    create_at: Instant,
}

impl Task {
    pub fn new(data: Bytes) -> Self {
        Task {
            code: TaskCode::UnknownError,
            data,
            create_at: Instant::now(),
        }
    }

    pub fn update(&mut self, code: TaskCode, data: &Bytes) {
        self.code = code;
        self.data = data.clone();
    }
}

#[derive(Debug)]
struct Processor {
    tasks: Arc<Mutex<TaskHub>>,
    batch_size: u32,
    wait_time: Duration,
    listener: UnixListener,
    receiver: Receiver<u32>,
    sender: Sender<u32>,
}

impl Processor {
    fn new(
        tasks: Arc<Mutex<TaskHub>>,
        batch_size: u32,
        wait_time: Duration,
        path: &Path,
        receiver: Receiver<u32>,
        sender: Sender<u32>,
    ) -> Self {
        println!("listen on {:?}", path);
        let listener = UnixListener::bind(path).unwrap();
        Processor {
            tasks,
            batch_size,
            wait_time,
            listener,
            receiver,
            sender,
        }
    }

    async fn run(&self) {
        loop {
            let tasks_clone = self.tasks.clone();
            let input_clone = self.receiver.clone();
            let output_clone = self.sender.clone();
            let batch_size = self.batch_size;
            let wait_time = self.wait_time;
            match self.listener.accept().await {
                Ok((stream, addr)) => {
                    println!("Accepted connection from {:?}", addr);
                    tokio::spawn(async move {
                        loop {
                            if receive_message(&stream, &tasks_clone, &output_clone)
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if send_message(
                                &stream,
                                &tasks_clone,
                                &input_clone,
                                batch_size,
                                wait_time,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Error accepting connection: {:?}", e);
                    break;
                }
            }
        }
    }
}

async fn read_exact(stream: &UnixStream, buf: &mut [u8]) -> Result<(), ProtocolError> {
    loop {
        match stream.try_read(buf) {
            Ok(0) => return Err(ProtocolError::SocketClosed),
            Ok(n) if n != buf.len() => return Err(ProtocolError::ReadIncomplete),
            Ok(_) => return Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                eprintln!("read socket err: {}", &e);
                return Err(ProtocolError::ReadError);
            }
        }
    }
}

async fn receive_message(
    stream: &UnixStream,
    tasks: &Arc<tokio::sync::Mutex<TaskHub>>,
    sender: &Sender<u32>,
) -> Result<(), ProtocolError> {
    if stream.readable().await.is_err() {
        return Err(ProtocolError::ReadError);
    }
    let mut flag_buf = [0u8; FLAG_U8_SIZE];
    let mut num_buf = [0u8; NUM_U8_SIZE];
    read_exact(stream, &mut flag_buf).await?;
    read_exact(stream, &mut num_buf).await?;
    let flag = u16::from_be_bytes(flag_buf);
    let num = u16::from_be_bytes(num_buf);

    let code = if flag & BIT_STATUS_OK > 0 {
        TaskCode::Normal
    } else if flag & BIT_STATUS_BAD_REQ > 0 {
        TaskCode::BadRequestError
    } else if flag & BIT_STATUS_VALIDATION_ERR > 0 {
        TaskCode::ValidationError
    } else if flag & BIT_STATUS_INTERNAL_ERR > 0 {
        TaskCode::InternalError
    } else {
        TaskCode::UnknownError
    };

    let mut id_buf = [0u8; TASK_ID_U8_SIZE];
    let mut length_buf = [0u8; LENGTH_U8_SIZE];
    let mut ids: Vec<u32> = Vec::new();
    let mut data: Vec<Bytes> = Vec::new();
    for _ in 0..num {
        read_exact(stream, &mut id_buf).await?;
        read_exact(stream, &mut length_buf).await?;
        let id = u32::from_be_bytes(id_buf);
        let length = u32::from_be_bytes(length_buf);
        let mut data_buf = vec![0u8; length as usize];
        read_exact(stream, &mut data_buf).await?;
        ids.push(id);
        data.push(data_buf.into());
    }

    // update tasks received from the stream
    {
        let mut tasks = tasks.lock().await;
        tasks.update_multi_tasks(code, &ids, &data);
    }

    // send normal tasks to the next channel
    match code {
        TaskCode::Normal => {
            for id in ids {
                if sender.send(id).await.is_err() {
                    return Err(ProtocolError::SendError);
                }
            }
        }
        _ => {
            println!("abnormal tasks: {:?}", &ids);
        }
    }
    Ok(())
}

async fn get_batch(receiver: &Receiver<u32>, batch_size: usize, batch_vec: &mut Vec<u32>) {
    loop {
        match receiver.recv().await {
            Ok(id) => {
                batch_vec.push(id);
            }
            Err(err) => {
                eprintln!("receive from channel error: {}", err);
            }
        }
        if batch_vec.len() == batch_size {
            break;
        }
    }
}

async fn send_message(
    stream: &UnixStream,
    tasks: &Arc<tokio::sync::Mutex<TaskHub>>,
    receiver: &Receiver<u32>,
    batch_size: u32,
    wait_time: Duration,
) -> Result<(), ProtocolError> {
    // get batch from the channel
    let mut batch: Vec<u32> = Vec::new();

    match receiver.recv().await {
        Ok(id) => {
            batch.push(id);
            // timing from receiving the first item
            if tokio::time::timeout(
                wait_time,
                get_batch(receiver, batch_size as usize, &mut batch),
            )
            .await
            .is_err()
            {
                println!(
                    "timeout before the batch is full: {}/{}",
                    batch.len(),
                    batch_size
                );
            }
        }
        Err(e) => {
            eprintln!("receive from channel error: {}", e);
            return Err(ProtocolError::ReceiveError);
        }
    }

    // send the batch tasks to the stream
    if stream.writable().await.is_err() {
        return Err(ProtocolError::WriteError);
    }

    Ok(())
}

#[derive(Debug)]
struct TaskHub {
    table: HashMap<u32, Task>,
    notifiers: HashMap<u32, oneshot::Sender<()>>,
    current_id: u32,
}

impl TaskHub {
    pub fn update_multi_tasks(&mut self, code: TaskCode, ids: &Vec<u32>, data: &Vec<Bytes>) {
        for i in 0..ids.len() {
            let task = self.table.get_mut(&ids[i]);
            match task {
                Some(task) => {
                    task.update(code, &data[i]);
                    match code {
                        TaskCode::Normal => {}
                        _ => {
                            if let Some(s) = self.notifiers.remove(&ids[i]) {
                                s.send(()).unwrap();
                            } else {
                                eprintln!("no notifier for task {}", &ids[i]);
                            }
                        }
                    }
                }
                None => {
                    eprintln!("cannot find id: {}", ids[i]);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Protocol {
    capacity: usize,
    path: String,
    batches: Vec<u32>,
    sender: Sender<u32>,
    receiver: Receiver<u32>,
    tasks: Arc<Mutex<TaskHub>>,
    wait_time: Duration,
    pub timeout: Duration,
}

impl Protocol {
    pub fn new(
        batches: Vec<u32>,
        unix_dir: &str,
        capacity: usize,
        timeout: Duration,
        wait_time: Duration,
    ) -> Self {
        let (sender, receiver) = bounded::<u32>(capacity);
        Protocol {
            capacity,
            path: unix_dir.to_string(),
            batches,
            sender,
            receiver,
            tasks: Arc::new(Mutex::new(TaskHub {
                table: HashMap::with_capacity(capacity),
                notifiers: HashMap::with_capacity(capacity),
                current_id: 0,
            })),
            timeout,
            wait_time,
        }
    }

    pub async fn run(&mut self) {
        let mut last_receiver = self.receiver.clone();
        let folder = Path::new(&self.path);
        if !folder.is_dir() {
            fs::create_dir(folder).unwrap();
        }

        for (i, batch) in self.batches.iter().enumerate() {
            let (sender, receiver) = bounded::<u32>(self.capacity);
            let processor = Processor::new(
                self.tasks.clone(),
                *batch,
                self.wait_time,
                &folder.join(format!("ipc_{:?}.socket", i)),
                last_receiver.clone(),
                sender.clone(),
            );
            tokio::spawn(async move {
                processor.run().await;
            });
            last_receiver = receiver.clone();
        }
        self.receiver = last_receiver;
    }

    pub async fn add_new_task(&self, data: Bytes, notifier: oneshot::Sender<()>) -> u32 {
        let mut tasks = self.tasks.lock().await;
        let id = tasks.current_id;
        tasks.table.insert(id, Task::new(data));
        tasks.notifiers.insert(id, notifier);
        let _ = tasks.current_id.wrapping_add(1);
        id
    }

    pub async fn get_task(&self, id: u32) -> Option<Task> {
        let mut tasks = self.tasks.lock().await;
        tasks.table.remove(&id)
    }
}
