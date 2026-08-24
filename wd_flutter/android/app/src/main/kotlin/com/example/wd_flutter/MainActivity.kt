package com.example.wd_flutter

import android.content.Intent
import android.media.projection.MediaProjectionManager
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    private val requestProjection = 9001

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        WdNative.init(applicationContext)

        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "wd_cast")
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "init" -> result.success(WdNative.init(applicationContext))
                    "hasToken" -> result.success(WdNative.hasToken())
                    "pair" -> {
                        val host = call.argument<String>("host")
                        val port = call.argument<Int>("port") ?: 48330
                        val code = call.argument<String>("code") ?: ""
                        if (host == null) {
                            result.error("args", "missing host", null)
                            return@setMethodCallHandler
                        }
                        Thread {
                            val r = WdNative.pair(host, port, code, "Android Phone")
                            runOnUiThread { result.success(r) }
                        }.start()
                    }
                    "startCast" -> {
                        val host = call.argument<String>("host")
                        val port = call.argument<Int>("port") ?: 48330
                        if (host == null) {
                            result.error("args", "missing host", null)
                            return@setMethodCallHandler
                        }
                        CastPrefs.host = host
                        CastPrefs.port = port
                        if (!WdNative.hasToken()) {
                            result.success(
                                mapOf("ok" to false, "state" to "unpaired", "error" to "not paired yet")
                            )
                            return@setMethodCallHandler
                        }
                        val mpm =
                            getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                        startActivityForResult(mpm.createScreenCaptureIntent(), requestProjection)
                        result.success(mapOf("ok" to true, "pending" to true))
                    }
                    "stopCast" -> {
                        CastService.stop(this)
                        WdNative.stopCast()
                        result.success(true)
                    }
                    "status" -> result.success(WdNative.status())
                    else -> result.notImplemented()
                }
            }

        EventChannel(flutterEngine.dartExecutor.binaryMessenger, "wd_cast/events")
            .setStreamHandler(object : EventChannel.StreamHandler {
                override fun onListen(args: Any?, events: EventChannel.EventSink?) {
                    CastBus.sink = events
                }

                override fun onCancel(args: Any?) {
                    CastBus.sink = null
                }
            })
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != requestProjection) return
        if (resultCode == RESULT_OK && data != null) {
            CastService.start(this, resultCode, data)
        } else {
            CastBus.post(CastEvent.PermissionDenied)
        }
    }
}
