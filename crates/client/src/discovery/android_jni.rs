//! Android JNI `WifiManager.MulticastLock` for LAN discovery on Horizon OS.
//!
//! Provides real JNI bindings on the Android target, and a stub on non-Android hosts that
//! reports the lock as unavailable.
//!
//! The `MulticastLock` object is kept as a JNI global reference so it survives across calls.
//! It is released on `release` and again on drop (the reference is kept until the Java release
//! succeeds, so a failed release can be retried), so a discovery that is dropped mid-browse
//! does not leave Wi-Fi multicast reception switched on.

use crate::discovery::multicast_lock::MulticastLock;
use crate::error::{ClientError, ClientResult};

/// Tag shown in `adb shell dumpsys wifi` for locks taken by this app.
#[cfg(target_os = "android")]
const MULTICAST_LOCK_TAG: &str = "linux-quest-discovery";

#[cfg(target_os = "android")]
pub struct AndroidMulticastLock {
    lock: Option<jni::objects::GlobalRef>,
}

#[cfg(target_os = "android")]
impl AndroidMulticastLock {
    pub fn new() -> Self {
        Self { lock: None }
    }
}

#[cfg(target_os = "android")]
impl Default for AndroidMulticastLock {
    fn default() -> Self {
        Self::new()
    }
}

/// Local reference slots reserved for one JNI call sequence. A native thread has no Java frame
/// that would free locals on return, so every sequence runs inside a local frame.
#[cfg(target_os = "android")]
const LOCAL_FRAME_CAPACITY: i32 = 16;

/// Converts a JNI error into a [`ClientError`], first describing and clearing any pending Java
/// exception. Calling further JNI functions with an exception pending is undefined behaviour,
/// and an uncleared exception would also surface on the next call from this thread.
#[cfg(target_os = "android")]
fn jni_error(env: &mut jni::JNIEnv, what: &str, e: jni::errors::Error) -> ClientError {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    ClientError::Android(format!("{what}: {e}"))
}

#[cfg(target_os = "android")]
impl AndroidMulticastLock {
    /// Creates the Java `MulticastLock`, pins it with a global reference, then acquires it.
    /// All local references die with the frame. The global reference is created before
    /// `acquire()` so a failure to create it cannot leave the Java lock held without a handle.
    fn create_and_acquire(env: &mut jni::JNIEnv) -> ClientResult<jni::objects::GlobalRef> {
        let ctx = ndk_context::android_context();
        let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };

        env.with_local_frame(LOCAL_FRAME_CAPACITY, |env| -> ClientResult<_> {
            // WifiManager wifi = (WifiManager) activity.getSystemService("wifi");
            let service_name = env
                .new_string("wifi")
                .map_err(|e| jni_error(env, "JNI new_string failed", e))?;
            let wifi = env
                .call_method(
                    &activity,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[(&service_name).into()],
                )
                .map_err(|e| jni_error(env, "getSystemService failed", e))?
                .l()
                .map_err(|e| jni_error(env, "Invalid WifiManager object", e))?;

            if wifi.is_null() {
                return Err(ClientError::Android("WifiManager service is null".into()));
            }

            // WifiManager.MulticastLock lock = wifi.createMulticastLock(TAG);
            let tag = env
                .new_string(MULTICAST_LOCK_TAG)
                .map_err(|e| jni_error(env, "JNI new_string tag failed", e))?;
            let lock = env
                .call_method(
                    &wifi,
                    "createMulticastLock",
                    "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
                    &[(&tag).into()],
                )
                .map_err(|e| jni_error(env, "createMulticastLock failed", e))?
                .l()
                .map_err(|e| jni_error(env, "MulticastLock conversion failed", e))?;

            if lock.is_null() {
                return Err(ClientError::Android(
                    "createMulticastLock returned null".into(),
                ));
            }

            // Pin first, acquire second.
            let global = env
                .new_global_ref(&lock)
                .map_err(|e| jni_error(env, "MulticastLock global ref failed", e))?;

            // lock.acquire();  (requires android.permission.CHANGE_WIFI_MULTICAST_STATE)
            env.call_method(&lock, "acquire", "()V", &[])
                .map_err(|e| jni_error(env, "MulticastLock.acquire failed", e))?;
            Ok(global)
        })
    }
}

#[cfg(target_os = "android")]
impl MulticastLock for AndroidMulticastLock {
    fn acquire(&mut self) -> ClientResult<()> {
        if self.lock.is_some() {
            return Ok(());
        }

        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            .map_err(|e| ClientError::Android(format!("Failed to get JavaVM: {e}")))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| ClientError::Android(format!("Failed to attach JNI thread: {e}")))?;

        self.lock = Some(Self::create_and_acquire(&mut env)?);
        Ok(())
    }

    fn release(&mut self) -> ClientResult<()> {
        // Keep the stored reference until the Java release succeeds, so a failed call can be
        // retried (by the next scan or by drop) instead of leaking a held lock.
        let Some(lock) = self.lock.as_ref() else {
            return Ok(());
        };

        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            .map_err(|e| ClientError::Android(format!("Failed to get JavaVM: {e}")))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| ClientError::Android(format!("Failed to attach JNI thread: {e}")))?;

        // lock.release();
        env.with_local_frame(LOCAL_FRAME_CAPACITY, |env| -> ClientResult<()> {
            env.call_method(lock.as_obj(), "release", "()V", &[])
                .map_err(|e| jni_error(env, "MulticastLock.release failed", e))?;
            Ok(())
        })?;
        self.lock = None;
        Ok(())
    }

    fn is_held(&self) -> bool {
        self.lock.is_some()
    }
}

#[cfg(target_os = "android")]
impl Drop for AndroidMulticastLock {
    fn drop(&mut self) {
        // Best effort: a failed JNI call here cannot be reported anywhere useful.
        let _ = self.release();
    }
}

/// Stub implementation when running on non-Android development hosts.
#[cfg(not(target_os = "android"))]
#[derive(Debug, Default, Clone)]
pub struct AndroidMulticastLock {
    _private: (),
}

#[cfg(not(target_os = "android"))]
impl AndroidMulticastLock {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(not(target_os = "android"))]
impl MulticastLock for AndroidMulticastLock {
    fn acquire(&mut self) -> ClientResult<()> {
        Err(ClientError::Android(
            "WifiManager.MulticastLock is only available on the Android / Horizon OS target".into(),
        ))
    }

    fn release(&mut self) -> ClientResult<()> {
        Ok(())
    }

    fn is_held(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "android"))]
    #[test]
    fn stub_reports_lock_unavailable_on_host() {
        let mut lock = AndroidMulticastLock::new();
        assert!(!lock.is_held());
        assert!(lock.acquire().is_err());
        assert!(!lock.is_held());
        assert!(lock.release().is_ok());
    }
}
