package com.linuxquest.client;

import android.app.NativeActivity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Bundle;
import android.os.Build;

/** Exclusive immersive side of the hybrid app. Home presentation releases its XR session. */
public final class VrActivity extends NativeActivity {
    private final BroadcastReceiver home = new BroadcastReceiver() {
        @Override public void onReceive(Context context, Intent intent) { finishAndRemoveTask(); }
    };

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        IntentFilter filter = new IntentFilter(getPackageName() + ".ENTER_HOME");
        if (Build.VERSION.SDK_INT >= 33) registerReceiver(home, filter, Context.RECEIVER_NOT_EXPORTED);
        else registerReceiver(home, filter, getPackageName() + ".MODE_SWITCH", null);
        sendBroadcast(new Intent(getPackageName() + ".ENTER_VR").setPackage(getPackageName()), getPackageName() + ".MODE_SWITCH");
    }

    @Override protected void onDestroy() {
        unregisterReceiver(home);
        super.onDestroy();
    }
}
