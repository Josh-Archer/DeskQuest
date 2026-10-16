//! Android JNI ClipboardManager implementation for Horizon OS on Meta Quest.
//!
//! Provides real JNI bindings on Android target, and a safe stub on non-Android hosts.

use crate::clipboard::manager::ClipboardManager;
use crate::error::ClientError;

#[cfg(target_os = "android")]
pub struct AndroidClipboardManager {
    _private: (),
}

#[cfg(target_os = "android")]
impl AndroidClipboardManager {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(target_os = "android")]
impl Default for AndroidClipboardManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "android")]
impl ClipboardManager for AndroidClipboardManager {
    fn get_text(&self) -> Result<String, ClientError> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            .map_err(|e| ClientError::Android(format!("Failed to get JavaVM: {e}")))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| ClientError::Android(format!("Failed to attach JNI thread: {e}")))?;

        let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };

        // String clipboardService = Context.CLIPBOARD_SERVICE ("clipboard")
        let service_name = env
            .new_string("clipboard")
            .map_err(|e| ClientError::Android(format!("JNI new_string failed: {e}")))?;

        // ClipboardManager clipboard = (ClipboardManager) activity.getSystemService(service_name);
        let clipboard = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&service_name).into()],
            )
            .map_err(|e| ClientError::Android(format!("getSystemService failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("Invalid clipboard object: {e}")))?;

        if clipboard.is_null() {
            return Ok(String::new());
        }

        // ClipData primaryClip = clipboard.getPrimaryClip();
        let clip_data = env
            .call_method(
                &clipboard,
                "getPrimaryClip",
                "()Landroid/content/ClipData;",
                &[],
            )
            .map_err(|e| ClientError::Android(format!("getPrimaryClip failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("ClipData conversion failed: {e}")))?;

        if clip_data.is_null() {
            return Ok(String::new());
        }

        // int itemCount = clip_data.getItemCount();
        let count = env
            .call_method(&clip_data, "getItemCount", "()I", &[])
            .map_err(|e| ClientError::Android(format!("getItemCount failed: {e}")))?
            .i()
            .map_err(|e| ClientError::Android(format!("getItemCount int failed: {e}")))?;

        if count == 0 {
            return Ok(String::new());
        }

        // ClipData.Item item = clip_data.getItemAt(0);
        let item = env
            .call_method(
                &clip_data,
                "getItemAt",
                "(I)Landroid/content/ClipData$Item;",
                &[0i32.into()],
            )
            .map_err(|e| ClientError::Android(format!("getItemAt failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("Item conversion failed: {e}")))?;

        // CharSequence text = item.coerceToText(activity);
        let text_seq = env
            .call_method(
                &item,
                "coerceToText",
                "(Landroid/content/Context;)Ljava/lang/CharSequence;",
                &[(&activity).into()],
            )
            .map_err(|e| ClientError::Android(format!("coerceToText failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("CharSequence failed: {e}")))?;

        if text_seq.is_null() {
            return Ok(String::new());
        }

        // String textStr = text_seq.toString();
        let text_jstr = env
            .call_method(&text_seq, "toString", "()Ljava/lang/String;", &[])
            .map_err(|e| ClientError::Android(format!("toString failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("String conversion failed: {e}")))?;

        let text: String = env
            .get_string(&jni::objects::JString::from(text_jstr))
            .map_err(|e| ClientError::Android(format!("get_string failed: {e}")))?
            .into();

        Ok(text)
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClientError> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            .map_err(|e| ClientError::Android(format!("Failed to get JavaVM: {e}")))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| ClientError::Android(format!("Failed to attach JNI thread: {e}")))?;

        let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };

        let service_name = env
            .new_string("clipboard")
            .map_err(|e| ClientError::Android(format!("JNI new_string failed: {e}")))?;

        let clipboard = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&service_name).into()],
            )
            .map_err(|e| ClientError::Android(format!("getSystemService failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("Invalid clipboard object: {e}")))?;

        if clipboard.is_null() {
            return Err(ClientError::Android(
                "ClipboardManager service is null".into(),
            ));
        }

        let label = env
            .new_string("linux-quest")
            .map_err(|e| ClientError::Android(format!("JNI new_string label failed: {e}")))?;
        let text_jstr = env
            .new_string(text)
            .map_err(|e| ClientError::Android(format!("JNI new_string text failed: {e}")))?;

        // ClipData clip = ClipData.newPlainText(label, text);
        let clip_class = env
            .find_class("android/content/ClipData")
            .map_err(|e| ClientError::Android(format!("find_class ClipData failed: {e}")))?;

        let clip = env
            .call_static_method(
                &clip_class,
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[(&label).into(), (&text_jstr).into()],
            )
            .map_err(|e| ClientError::Android(format!("newPlainText failed: {e}")))?
            .l()
            .map_err(|e| ClientError::Android(format!("ClipData return failed: {e}")))?;

        // clipboard.setPrimaryClip(clip);
        env.call_method(
            &clipboard,
            "setPrimaryClip",
            "(Landroid/content/ClipData;)V",
            &[(&clip).into()],
        )
        .map_err(|e| ClientError::Android(format!("setPrimaryClip failed: {e}")))?;

        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "android-jni"
    }
}

/// Stub implementation when running on non-Android development hosts.
#[cfg(not(target_os = "android"))]
#[derive(Debug, Default, Clone)]
pub struct AndroidClipboardManager {
    _private: (),
}

#[cfg(not(target_os = "android"))]
impl AndroidClipboardManager {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(not(target_os = "android"))]
impl ClipboardManager for AndroidClipboardManager {
    fn get_text(&self) -> Result<String, ClientError> {
        Err(ClientError::Android(
            "Android ClipboardManager JNI is only available on Android / Horizon OS target".into(),
        ))
    }

    fn set_text(&mut self, _text: &str) -> Result<(), ClientError> {
        Err(ClientError::Android(
            "Android ClipboardManager JNI is only available on Android / Horizon OS target".into(),
        ))
    }

    fn is_available(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "android-jni (stub)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stubbed_clipboard_manager_on_host() {
        let mut manager = AndroidClipboardManager::new();
        assert!(!manager.is_available());
        assert_eq!(manager.name(), "android-jni (stub)");
        assert!(manager.get_text().is_err());
        assert!(manager.set_text("test").is_err());
    }
}
