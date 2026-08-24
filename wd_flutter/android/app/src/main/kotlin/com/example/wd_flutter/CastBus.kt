package com.example.wd_flutter

import android.os.Handler
import android.os.Looper
import io.flutter.plugin.common.EventChannel

object CastPrefs {
    @Volatile var host: String = ""
    @Volatile var port: Int = 48330
}

enum class CastEvent {
    PermissionDenied,
    SessionRejected,
    Streaming,
    Stopped,
}

object CastBus {
    @Volatile var sink: EventChannel.EventSink? = null
    private val main = Handler(Looper.getMainLooper())

    fun post(event: CastEvent, detail: String = "") {
        main.post {
            sink?.success(if (detail.isEmpty()) event.name else "${event.name}:$detail")
        }
    }
}
