pub(crate) mod reader;
pub mod whitelist;

use crate::push::{self, PushService};
use reader::Reader;
use std::{
    net::SocketAddr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use whitelist::Entry;

pub fn start(
    address: SocketAddr,
    root: &Path,
    key: &str,
    whitelist: Vec<Entry>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("无法启动运行时：{e}"))?;
    let root = root.to_owned();
    let key = key.to_owned();
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|e| format!("无法监听 {address}：{e}"))?;
        let actual = listener.local_addr().map_err(|e| e.to_string())?;
        let service = PushService::new(whitelist).with_history(root.clone(), key.clone());
        let publisher = service.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_worker = stopping.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let mut reader = match Reader::open(&root, &key) {
                Ok(reader) => reader,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            while !stop_worker.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                if stop_worker.load(Ordering::Relaxed) {
                    break;
                }
                match reader.scan(false) {
                    Ok(messages) => {
                        for message in messages {
                            if let Err(error) = publisher.publish(&message) {
                                eprintln!("消息推送失败：{error}");
                            }
                        }
                    }
                    Err(error) => eprintln!("监控查询失败，将重试：{error}"),
                }
            }
        });
        if let Err(error) = ready_rx.await.map_err(|_| "监控初始化异常")? {
            let _ = worker.await;
            return Err(error);
        }
        println!("新消息监控已启动（每 2 秒查询一次），SSE：http://{actual}/api/events");
        println!("仅推送启动后的新消息；按 Ctrl+C 停止。");
        let result = tokio::select! {
            result = std::future::IntoFuture::into_future(axum::serve(listener, push::router(service))) => result.map_err(|e| format!("SSE 服务失败：{e}")),
            signal = tokio::signal::ctrl_c() => signal.map_err(|e| format!("无法监听退出信号：{e}")),
        };
        stopping.store(true, Ordering::Relaxed);
        worker.await.map_err(|e| format!("监控任务异常：{e}"))?;
        result
    })
}
