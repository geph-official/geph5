package io.geph.geph5

import android.app.NativeActivity
import android.content.Intent
import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity

class LauncherActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Your Java code goes here
        initJvm()

        // Launch the NativeActivity
        startNativeActivity()

        // Finish this activity
        finish()
    }

    private fun initJvm() {
        NativeBridge().setjvm()
    }

    private fun startNativeActivity() {
        val intent = Intent(this, NativeActivity::class.java)
        startActivity(intent)
    }
}

class NativeBridge {
    companion object {
        init {
            System.loadLibrary("na_egui")
        }
    }

    external fun setjvm()
}