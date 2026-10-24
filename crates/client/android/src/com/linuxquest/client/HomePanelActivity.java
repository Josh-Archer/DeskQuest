package com.linuxquest.client;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.graphics.Color;
import android.os.Bundle;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.text.Editable;
import android.text.InputType;
import android.text.SpannableStringBuilder;
import android.view.GestureDetector;
import android.view.Gravity;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewGroup;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.TextView;
import org.json.JSONObject;

/** A resizable desktop stream panel in Horizon Home, with system hand/touch input. */
public final class HomePanelActivity extends Activity implements SurfaceHolder.Callback {
    static { System.loadLibrary("linux_quest_client"); }

    private static native boolean nativeStart(Activity activity, Surface surface);
    private static native void nativeStop();
    private static native String nativeStatus();
    private static native void nativePointer(float x, float y);
    private static native void nativeButton(int button, boolean down);
    private static native void nativeScroll(float x, float y);
    private static native boolean nativeKey(int key, int meta, boolean down);
    private static native void nativeText(String text);
    private static native void nativeReleaseInput();

    private final Handler main = new Handler();
    private final HandlerThread streamThread = new HandlerThread("deskquest-home-control");
    private Handler stream;
    private DesktopSurface desktop;
    private FrameLayout canvas;
    private TextView status;
    private volatile boolean visible;
    private volatile int generation;
    private boolean interactive;
    private boolean keyboardVisible;
    private int videoWidth = 1920;
    private int videoHeight = 1080;
    private final BroadcastReceiver immersive = new BroadcastReceiver() {
        @Override public void onReceive(Context context, Intent intent) { finishAndRemoveTask(); }
    };

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        streamThread.start();
        stream = new Handler(streamThread.getLooper());
        IntentFilter filter = new IntentFilter(getPackageName() + ".ENTER_VR");
        if (Build.VERSION.SDK_INT >= 33) registerReceiver(immersive, filter, Context.RECEIVER_NOT_EXPORTED);
        else registerReceiver(immersive, filter, getPackageName() + ".MODE_SWITCH", null);
        sendBroadcast(new Intent(getPackageName() + ".ENTER_HOME").setPackage(getPackageName()), getPackageName() + ".MODE_SWITCH");

        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(Color.rgb(12, 15, 22));
        LinearLayout toolbar = new LinearLayout(this);
        toolbar.setGravity(Gravity.CENTER_VERTICAL);
        toolbar.setPadding(dp(12), 0, dp(8), 0);
        ImageView logo = new ImageView(this);
        logo.setImageDrawable(getApplicationInfo().loadIcon(getPackageManager()));
        logo.setScaleType(ImageView.ScaleType.FIT_CENTER);
        logo.setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_NO);
        LinearLayout.LayoutParams logoLayout = new LinearLayout.LayoutParams(dp(36), dp(36));
        logoLayout.setMarginEnd(dp(10));
        toolbar.addView(logo, logoLayout);
        TextView title = new TextView(this);
        title.setText("DeskQuest");
        title.setTextColor(Color.WHITE);
        title.setTextSize(18);
        toolbar.addView(title, new LinearLayout.LayoutParams(0, dp(52), 1));
        addButton(toolbar, "Keyboard", () -> {
            desktop.requestFocus();
            InputMethodManager ime = (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
            keyboardVisible = !keyboardVisible;
            if (keyboardVisible) ime.showSoftInput(desktop, InputMethodManager.SHOW_IMPLICIT);
            else ime.hideSoftInputFromWindow(desktop.getWindowToken(), 0);
        });
        addButton(toolbar, "Reconnect", this::startStream);
        addButton(toolbar, "Open VR", () -> {
            ++generation;
            stream.post(() -> {
                nativeStop();
                main.post(() -> {
                    if (isFinishing() || isDestroyed()) return;
                    startActivity(new Intent(this, VrActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
                    finishAndRemoveTask();
                });
            });
        });
        root.addView(toolbar);
        canvas = new FrameLayout(this);
        desktop = new DesktopSurface();
        desktop.getHolder().addCallback(this);
        canvas.addView(desktop, new FrameLayout.LayoutParams(1, 1, Gravity.CENTER));
        canvas.addOnLayoutChangeListener((v, l, t, r, b, ol, ot, or, ob) -> fitDesktop());
        root.addView(canvas, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));
        status = new TextView(this);
        status.setText("Connecting to your desktop…");
        status.setTextColor(Color.LTGRAY);
        status.setPadding(dp(12), dp(6), dp(12), dp(6));
        root.addView(status);
        setContentView(root);
    }

    private int dp(int value) { return Math.round(value * getResources().getDisplayMetrics().density); }
    private void addButton(LinearLayout toolbar, String text, Runnable action) {
        Button button = new Button(this);
        button.setText(text);
        button.setOnClickListener(v -> action.run());
        // Pointer and pinch only: a button that took keyboard focus would swallow typing meant
        // for the desktop (Space or Enter would press it).
        button.setFocusable(false);
        button.setFocusableInTouchMode(false);
        toolbar.addView(button);
    }

    /**
     * Every key goes to the desktop while it is connected, whichever view has focus: a
     * keyboard paired with or projected by Horizon (Surface Keyboard) types into the host.
     */
    @Override public boolean dispatchKeyEvent(KeyEvent event) {
        if (desktop != null && desktop.forwardKey(event)) return true;
        return super.dispatchKeyEvent(event);
    }

    private void fitDesktop() {
        int width = canvas.getWidth(), height = canvas.getHeight();
        if (width <= 0 || height <= 0 || videoWidth <= 0 || videoHeight <= 0) return;
        float scale = Math.min((float) width / videoWidth, (float) height / videoHeight);
        int w = Math.max(1, Math.round(videoWidth * scale));
        int h = Math.max(1, Math.round(videoHeight * scale));
        ViewGroup.LayoutParams layout = desktop.getLayoutParams();
        if (layout.width != w || layout.height != h) {
            layout.width = w;
            layout.height = h;
            desktop.setLayoutParams(layout);
        }
    }

    private void startStream() {
        final int token = ++generation;
        final Surface surface = desktop.getHolder().getSurface();
        if (!visible || !surface.isValid()) return;
        stream.post(() -> {
            if (token != generation || !visible || !surface.isValid()) return;
            nativeStart(this, surface);
        });
    }

    @Override public void surfaceCreated(SurfaceHolder holder) { startStream(); }
    @Override public void surfaceChanged(SurfaceHolder holder, int format, int w, int h) {}
    @Override public void surfaceDestroyed(SurfaceHolder holder) {
        ++generation;
        interactive = false;
        stream.post(HomePanelActivity::nativeStop);
    }
    @Override protected void onStart() {
        super.onStart();
        visible = true;
        startStream();
        main.post(pollStatus);
    }
    @Override protected void onPause() {
        interactive = false;
        desktop.cancelInteraction();
        nativeReleaseInput();
        super.onPause();
    }
    @Override protected void onStop() {
        visible = false;
        ++generation;
        interactive = false;
        main.removeCallbacks(pollStatus);
        stream.post(HomePanelActivity::nativeStop);
        super.onStop();
    }
    @Override protected void onDestroy() {
        unregisterReceiver(immersive);
        stream.post(HomePanelActivity::nativeStop);
        streamThread.quitSafely();
        super.onDestroy();
    }
    @Override public void onWindowFocusChanged(boolean focused) {
        super.onWindowFocusChanged(focused);
        if (focused && desktop != null) desktop.requestFocus();
        if (!focused) nativeReleaseInput();
    }
    private final Runnable pollStatus = new Runnable() {
        @Override public void run() {
            if (!visible) return;
            try {
                JSONObject value = new JSONObject(nativeStatus());
                status.setText(value.optString("message", "Connecting…"));
                boolean ready = value.optBoolean("input_ready");
                if (ready && !interactive && !desktop.hasFocus()) desktop.requestFocus();
                interactive = ready;
                int w = value.optInt("width", 1920), h = value.optInt("height", 1080);
                if (w != videoWidth || h != videoHeight) {
                    videoWidth = w; videoHeight = h; fitDesktop();
                }
            } catch (Exception error) { status.setText("Unable to read desktop status. Tap Reconnect."); }
            main.postDelayed(this, 500);
        }
    };

    private final class DesktopSurface extends SurfaceView {
        private final GestureDetector gestures;
        private boolean dragging;
        DesktopSurface() {
            super(HomePanelActivity.this);
            setFocusable(true);
            setFocusableInTouchMode(true);
            gestures = new GestureDetector(HomePanelActivity.this, new GestureDetector.SimpleOnGestureListener() {
                @Override public boolean onDown(MotionEvent e) { point(e); return true; }
                @Override public boolean onSingleTapUp(MotionEvent e) {
                    point(e); nativeButton(0, true); nativeButton(0, false); performClick(); return true;
                }
                @Override public void onLongPress(MotionEvent e) {
                    if (interactive) { point(e); nativeButton(0, true); dragging = true; }
                }
                @Override public boolean onScroll(MotionEvent first, MotionEvent current, float dx, float dy) {
                    if (!dragging) nativeScroll(dx / dp(48), -dy / dp(48));
                    return true;
                }
            });
        }
        private void point(MotionEvent e) {
            nativePointer(e.getX() / Math.max(1, getWidth()), e.getY() / Math.max(1, getHeight()));
        }
        void cancelInteraction() {
            dragging = false;
            long now = android.os.SystemClock.uptimeMillis();
            MotionEvent cancel = MotionEvent.obtain(now, now, MotionEvent.ACTION_CANCEL, 0, 0, 0);
            gestures.onTouchEvent(cancel);
            cancel.recycle();
        }
        @Override public boolean performClick() { super.performClick(); return true; }
        @Override public boolean onTouchEvent(MotionEvent e) {
            if (!interactive) return true;
            requestFocus();
            int action = e.getActionMasked();
            if (e.isFromSource(InputDevice.SOURCE_MOUSE)
                    && e.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE) {
                point(e);
                if (action == MotionEvent.ACTION_DOWN && (e.getButtonState() & MotionEvent.BUTTON_PRIMARY) != 0) nativeButton(0, true);
                if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) nativeButton(0, false);
                return true;
            }
            if (dragging) {
                point(e);
                if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
                    nativeButton(0, false); dragging = false;
                }
            } else gestures.onTouchEvent(e);
            return true;
        }
        @Override public boolean onGenericMotionEvent(MotionEvent e) {
            if (!interactive) return super.onGenericMotionEvent(e);
            point(e);
            if (e.getActionMasked() == MotionEvent.ACTION_SCROLL) {
                nativeScroll(e.getAxisValue(MotionEvent.AXIS_HSCROLL), e.getAxisValue(MotionEvent.AXIS_VSCROLL));
                return true;
            }
            if (e.getActionMasked() == MotionEvent.ACTION_BUTTON_PRESS || e.getActionMasked() == MotionEvent.ACTION_BUTTON_RELEASE) {
                int button = e.getActionButton();
                if (button == MotionEvent.BUTTON_SECONDARY || button == MotionEvent.BUTTON_TERTIARY) {
                    nativeButton(button == MotionEvent.BUTTON_SECONDARY ? 1 : 2,
                            e.getActionMasked() == MotionEvent.ACTION_BUTTON_PRESS);
                    return true;
                }
            }
            return e.getActionMasked() == MotionEvent.ACTION_HOVER_MOVE || super.onGenericMotionEvent(e);
        }
        boolean forwardKey(KeyEvent event) {
            if (!interactive || event.isSystem() || event.getAction() == KeyEvent.ACTION_MULTIPLE) return false;
            if ((event.isAltPressed() && event.getKeyCode() == KeyEvent.KEYCODE_TAB)
                    || (event.isCtrlPressed() && event.isAltPressed() && event.getKeyCode() >= KeyEvent.KEYCODE_F1 && event.getKeyCode() <= KeyEvent.KEYCODE_F12)) return false;
            if (event.isCtrlPressed() && event.isAltPressed() && event.getKeyCode() == KeyEvent.KEYCODE_ESCAPE) {
                nativeReleaseInput();
                ((InputMethodManager) getSystemService(INPUT_METHOD_SERVICE)).hideSoftInputFromWindow(getWindowToken(), 0);
                keyboardVisible = false;
                clearFocus(); return true;
            }
            return nativeKey(event.getKeyCode(), event.getMetaState(), event.getAction() == KeyEvent.ACTION_DOWN);
        }
        @Override public boolean onKeyDown(int key, KeyEvent e) { return forwardKey(e) || super.onKeyDown(key, e); }
        @Override public boolean onKeyUp(int key, KeyEvent e) { return forwardKey(e) || super.onKeyUp(key, e); }
        @Override public boolean onCheckIsTextEditor() { return true; }
        @Override public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE;
            info.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_ACTION_NONE;
            return new BaseInputConnection(this, true) {
                private final Editable editable = new SpannableStringBuilder();
                @Override public Editable getEditable() { return editable; }
                @Override public boolean commitText(CharSequence text, int cursor) {
                    nativeText(text.toString()); editable.clear(); return true;
                }
                @Override public boolean finishComposingText() {
                    if (BaseInputConnection.getComposingSpanStart(editable) >= 0) {
                        nativeText(editable.toString()); editable.clear();
                    }
                    return super.finishComposingText();
                }
                @Override public boolean sendKeyEvent(KeyEvent event) { return forwardKey(event); }
                @Override public boolean deleteSurroundingText(int before, int after) {
                    // Edits to uncommitted IME composition belong to the local editor.
                    if (editable.length() > 0) return super.deleteSurroundingText(before, after);
                    for (int i = 0; i < Math.min(before, 256); i++) {
                        nativeKey(KeyEvent.KEYCODE_DEL, 0, true); nativeKey(KeyEvent.KEYCODE_DEL, 0, false);
                    }
                    for (int i = 0; i < Math.min(after, 256); i++) {
                        nativeKey(KeyEvent.KEYCODE_FORWARD_DEL, 0, true); nativeKey(KeyEvent.KEYCODE_FORWARD_DEL, 0, false);
                    }
                    return true;
                }
            };
        }
    }
}
