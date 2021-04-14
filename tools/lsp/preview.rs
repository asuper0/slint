/* LICENSE BEGIN
    This file is part of the SixtyFPS Project -- https://sixtyfps.io
    Copyright (c) 2020 Olivier Goffart <olivier.goffart@sixtyfps.io>
    Copyright (c) 2020 Simon Hausmann <simon.hausmann@sixtyfps.io>

    SPDX-License-Identifier: GPL-3.0-only
    This file is also available under commercial licensing terms.
    Please contact info@sixtyfps.io for more information.
LICENSE END */

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Wake;

use lsp_server::Message;
use lsp_types::notification::Notification;

use crate::lsp_ext::{Health, ServerStatusNotification, ServerStatusParams};

struct FutureRunner {
    fut: Mutex<Option<Pin<Box<dyn Future<Output = ()>>>>>,
}

/// Safety: the future is only going to be run in the UI thread
unsafe impl Send for FutureRunner {}
/// Safety: the future is only going to be run in the UI thread
unsafe impl Sync for FutureRunner {}

impl Wake for FutureRunner {
    fn wake(self: Arc<Self>) {
        sixtyfps_rendering_backend_default::backend().post_event(Box::new(move || {
            let waker = self.clone().into();
            let mut cx = std::task::Context::from_waker(&waker);
            let mut fut_opt = self.fut.lock().unwrap();
            if let Some(fut) = &mut *fut_opt {
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(_) => *fut_opt = None,
                    std::task::Poll::Pending => {}
                }
            }
        }));
    }
}

fn run_in_ui_thread(fut: Pin<Box<dyn Future<Output = ()>>>) {
    Arc::new(FutureRunner { fut: Mutex::new(Some(fut)) }).wake()
}

pub fn start_ui_event_loop() {
    sixtyfps_rendering_backend_default::backend()
        .run_event_loop(sixtyfps_corelib::backend::EventLoopQuitBehavior::QuitOnlyExplicitly);
}

pub fn quit_ui_event_loop() {
    sixtyfps_rendering_backend_default::backend().post_event(Box::new(|| {
        sixtyfps_rendering_backend_default::backend().quit_event_loop();
    }));
}

pub enum PostLoadBehavior {
    ShowAfterLoad,
    DoNothing,
}

pub fn load_preview(
    sender: crossbeam_channel::Sender<Message>,
    path: std::path::PathBuf,
    post_load_behavior: PostLoadBehavior,
) {
    run_in_ui_thread(Box::pin(
        async move { reload_preview(sender, &path, post_load_behavior).await },
    ));
}

#[derive(Default)]
struct ContentCache {
    source_code: HashMap<PathBuf, String>,
    dependency: HashSet<PathBuf>,
    current_root: PathBuf,
    sender: Option<crossbeam_channel::Sender<Message>>,
}

static CONTENT_CACHE: once_cell::sync::OnceCell<Mutex<ContentCache>> =
    once_cell::sync::OnceCell::new();

pub fn set_contents(path: &Path, content: String) {
    let mut cache = CONTENT_CACHE.get_or_init(Default::default).lock().unwrap();
    cache.source_code.insert(path.to_owned(), content);
    if cache.dependency.contains(path) {
        let current_root = cache.current_root.clone();
        let sender = cache.sender.take();
        drop(cache);
        if let Some(sender) = sender {
            load_preview(sender, current_root, PostLoadBehavior::DoNothing);
        }
    }
}

/// If the file is in the cache, returns it.
/// In any was, register it as a dependency
fn get_file_from_cache(path: PathBuf) -> Option<String> {
    let mut cache = CONTENT_CACHE.get_or_init(Default::default).lock().unwrap();
    let r = cache.source_code.get(&path).map(|r| r.clone());
    cache.dependency.insert(path);
    r
}

async fn reload_preview(
    sender: crossbeam_channel::Sender<Message>,
    path: &std::path::Path,
    post_load_behavior: PostLoadBehavior,
) {
    send_notification(&sender, "Loading Preview…", Health::Ok);

    {
        let mut cache = CONTENT_CACHE.get_or_init(Default::default).lock().unwrap();
        cache.dependency.clear();
        cache.current_root = path.to_owned();
    }

    let mut builder = sixtyfps_interpreter::ComponentCompiler::new();
    builder.set_file_loader(|path| {
        let path = path.to_owned();
        Box::pin(async move { get_file_from_cache(path).map(Result::Ok) })
    });

    let compiled = if let Some(from_cache) = get_file_from_cache(path.to_owned()) {
        builder.build_from_source(from_cache, path.to_owned()).await
    } else {
        builder.build_from_path(path).await
    };

    if let Some(compiled) = compiled {
        #[derive(Default)]
        struct PreviewState {
            handle: Option<sixtyfps_interpreter::ComponentInstance>,
        }
        thread_local! {static PREVIEW_STATE: std::cell::RefCell<PreviewState> = Default::default();}
        PREVIEW_STATE.with(|preview_state| {
            let mut preview_state = preview_state.borrow_mut();
            if let Some(handle) = preview_state.handle.take() {
                let window = handle.window();
                let handle = compiled.create_with_existing_window(window);
                match post_load_behavior {
                    PostLoadBehavior::ShowAfterLoad => handle.show(),
                    PostLoadBehavior::DoNothing => {}
                }
                preview_state.handle = Some(handle);
            } else {
                let handle = compiled.create();
                handle.show();
                preview_state.handle = Some(handle);
            }
        });
        send_notification(&sender, "Preview Loaded", Health::Ok);
    } else {
        send_notification(&sender, "Preview not upated", Health::Error);
    }
    CONTENT_CACHE.get_or_init(Default::default).lock().unwrap().sender.replace(sender);
}

fn send_notification(sender: &crossbeam_channel::Sender<Message>, arg: &str, health: Health) {
    sender
        .send(Message::Notification(lsp_server::Notification::new(
            ServerStatusNotification::METHOD.into(),
            ServerStatusParams { health, quiescent: false, message: Some(arg.into()) },
        )))
        .unwrap_or_else(|e| eprintln!("Error sending notification: {:?}", e));
}
