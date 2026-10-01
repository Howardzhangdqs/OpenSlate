package dev.openslate.mobile

import android.app.Application

class OpenSlateApp : Application() {
    override fun onCreate() {
        super.onCreate()
        instance = this
    }

    companion object {
        lateinit var instance: OpenSlateApp
            private set
    }
}
