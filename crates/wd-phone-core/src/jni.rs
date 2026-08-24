//! JNI exports: `com.example.wd_flutter.WdNative`

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use jni::objects::{JClass, JString};
use jni::sys::jboolean;
use jni::JNIEnv;
use tokio::sync::{mpsc, watch};

use crate::session;
use crate::session::Shared;

struct Global {
    rt: tokio::runtime::Runtime,
    store_dir: PathBuf,
    shared: Arc<Shared>,
    session: Mutex<Option<session::SessionHandle>>,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

fn global() -> &'static Global {
    GLOBAL.get().expect("nativeInit not called")
}

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn new_jstring(env: &mut JNIEnv, s: &str) -> jni::sys::jstring {
    env.new_string(s)
        .map(|v| v.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    store_dir: JString,
) -> jboolean {
    let dir = jstr(&mut env, &store_dir);
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return 0,
    };
    let ok = GLOBAL
        .set(Global {
            rt,
            store_dir: PathBuf::from(dir),
            shared: Arc::new(Shared::default()),
            session: std::sync::Mutex::new(None),
        })
        .is_ok();
    if ok { 1 } else { 1 } // idempotent re-init is fine
}

/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeHasToken(
    mut env: JNIEnv,
    _class: JClass,
) -> jboolean {
    let g = global();
    if crate::store::load(&g.store_dir).is_some() { 1 } else { 0 }
}

/// Pair with a receiver; returns JSON `{ok, error?, deviceId?}`.
/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativePair(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: i32,
    code: JString,
    name: JString,
) -> jni::sys::jstring {
    let host = jstr(&mut env, &host);
    let code = jstr(&mut env, &code);
    let name = jstr(&mut env, &name);
    let g = global();

    let res: anyhow::Result<String> = g.rt.block_on(async move {
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| anyhow::anyhow!("bad address: {e}"))?;
        let conn = crate::proto::connect(addr).await?;
        let outcome = crate::proto::pair_with_receiver(&conn, &name, &code).await?;
        crate::store::save(
            &global().store_dir,
            &crate::store::DeviceToken {
                device_id: outcome.device_id.clone(),
                device_token: outcome.device_token.clone(),
            },
        )?;
        Ok(outcome.device_id)
    });

    match res {
        Ok(device_id) => new_jstring(
            &mut env,
            &serde_json::json!({"ok": true, "deviceId": device_id}).to_string(),
        ),
        Err(e) => new_jstring(
            &mut env,
            &serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
        ),
    }
}

/// Start casting; returns JSON `{ok, error?}` after SessionAnswer (or error).
/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeStartCast(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: i32,
    name: JString,
    width: i32,
    height: i32,
    fps: i32,
    bitrate_kbps: i32,
) -> jni::sys::jstring {
    let host = jstr(&mut env, &host);
    let name = jstr(&mut env, &name);
    let g = global();

    // Refuse to double-start.
    {
        let mut slot = g.session.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return new_jstring(
                &mut env,
                &serde_json::json!({"ok": false, "error": "cast already running"}).to_string(),
            );
        }
    }
    let token = match crate::store::load(&g.store_dir) {
        Some(t) => t.device_token,
        None => {
            return new_jstring(
                &mut env,
                &serde_json::json!({"ok": false, "error": "not paired yet"}).to_string(),
            )
        }
    };

    let shared = Arc::clone(&g.shared);
    shared.set_state("connecting");
    let (video_tx, video_rx) = mpsc::channel::<(Vec<u8>, bool, u64)>(96);
    let (stop_tx, stop_rx) = watch::channel(false);

    let addr_res: anyhow::Result<SocketAddr> =
        format!("{host}:{port}").parse().map_err(Into::into);

    let join = {
        let shared = Arc::clone(&shared);
        let name = name.clone();
        g.rt.spawn(async move {
            match addr_res {
                Ok(addr) => {
                    let r = session::run(
                        addr,
                        &name,
                        &token,
                        width.clamp(2, 4096) as u16,
                        height.clamp(2, 4096) as u16,
                        fps.clamp(1, 60) as u8,
                        bitrate_kbps.clamp(500, 25000) as u32,
                        shared,
                        video_rx,
                        stop_rx,
                    )
                    .await;
                    if let Err(e) = r {
                        tracing_or_log(&format!("{e:#}"));
                    }
                }
                Err(e) => tracing_or_log(&format!("{e:#}")),
            }
        })
    };

    let handle = session::SessionHandle {
        shared: Arc::clone(&g.shared),
        video_tx,
        stop_tx,
    };
    {
        let mut slot = g.session.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(handle);
    }
    let _ = join; // detached; status polled via nativeStatus

    // Wait briefly for offer/answer so the UI gets an immediate verdict.
    for _ in 0..80 {
        let state = g
            .shared
            .state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        match state.as_str() {
            "streaming" => break,
            "error" => break,
            "stopped" => break,
            _ => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    new_jstring(&mut env, &g.shared.snapshot())
}

#[allow(dead_code)]
fn tracing_or_log(msg: &str) {
    eprintln!("wd-phone-core: {msg}");
}

/// Push one MediaCodec buffer. Returns true when accepted.
/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeSendVideo(
    mut env: JNIEnv,
    _class: JClass,
    data: jni::sys::jbyteArray,
    is_config: jboolean,
    pts_us: i64,
) -> jboolean {
    let g = global();
    use jni::objects::JByteArray;
    let arr = unsafe { JByteArray::from_raw(data) };
    let bytes: Vec<u8> = match env.convert_byte_array(arr) {
        Ok(v) => v.into_iter().map(|b| b as u8).collect(),
        Err(_) => return 0,
    };
    let accepted = {
        let mut slot = g.session.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(h) => h.push(bytes, is_config != 0, pts_us.max(0) as u64),
            None => false,
        }
    };
    if accepted { 1 } else { 0 }
}

/// Poll status JSON `{state,error?,sent,dropped,keyframe}`.
/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeStatus(
    mut env: JNIEnv,
    _class: JClass,
) -> jni::sys::jstring {
    let g = global();
    new_jstring(&mut env, &g.shared.snapshot())
}

/// Consume the keyframe flag (Kotlin calls this each frame tick).
/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeTakeKeyframe(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    let g = global();
    if g.shared.keyframe.swap(false, std::sync::atomic::Ordering::Relaxed) {
        1
    } else {
        0
    }
}

/// Stop the current cast (Bye + teardown).
/// # Safety
/// Called from JVM.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_example_wd_1flutter_WdNative_nativeStopCast(
    mut _env: JNIEnv,
    _class: JClass,
) {
    let g = global();
    let mut slot = g.session.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = slot.take() {
        h.stop();
    }
}
