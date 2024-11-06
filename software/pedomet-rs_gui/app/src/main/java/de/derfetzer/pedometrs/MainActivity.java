package de.derfetzer.pedometrs;

import androidx.appcompat.app.AppCompatActivity;
import androidx.core.graphics.Insets;
import androidx.core.view.DisplayCutoutCompat;
import androidx.core.view.ViewCompat;
import androidx.core.view.WindowCompat;
import androidx.core.view.WindowInsetsCompat;
import androidx.core.view.WindowInsetsControllerCompat;

import com.google.androidgamesdk.GameActivity;

import android.os.Bundle;
import android.content.pm.PackageManager;
import android.os.Build.VERSION;
import android.os.Build.VERSION_CODES;
import android.os.Bundle;
import android.util.Log;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowManager;

public class MainActivity extends GameActivity {

    static {
        // Load the STL first to workaround issues on old Android versions:
        // "if your app targets a version of Android earlier than Android 4.3
        // (Android API level 18),
        // and you use libc++_shared.so, you must load the shared library before any other
        // library that depends on it."
        // See https://developer.android.com/ndk/guides/cpp-support#shared_runtimes
        //System.loadLibrary("c++_shared");

        // Load the native library.
        // The name "android-game" depends on your CMake configuration, must be
        // consistent here and inside AndroidManifect.xml
        System.loadLibrary("pedometrs");
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        // From https://github.com/podusowski/walkers/pull/175
        //
        // Shrink view so it does not get covered by insets.
        View content = getWindow().getDecorView().findViewById(android.R.id.content);
        ViewCompat.setOnApplyWindowInsetsListener(content, (v, windowInsets) -> {
          Insets insets = windowInsets.getInsets(WindowInsetsCompat.Type.systemBars());

          ViewGroup.MarginLayoutParams mlp = (ViewGroup.MarginLayoutParams) v.getLayoutParams();
          mlp.topMargin = insets.top;
          mlp.leftMargin = insets.left;
          mlp.bottomMargin = insets.bottom;
          mlp.rightMargin = insets.right;
          v.setLayoutParams(mlp);

          return WindowInsetsCompat.CONSUMED;
        });

        WindowCompat.setDecorFitsSystemWindows(getWindow(), true);
        super.onCreate(savedInstanceState);
    }

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        // From https://github.com/podusowski/walkers/pull/175
        //
        // Offset the location so it fits the view with margins caused by insets.

        int[] location = new int[2];
        findViewById(android.R.id.content).getLocationOnScreen(location);
        event.offsetLocation(-location[0], -location[1]);
        return super.onTouchEvent(event);
    }
}
