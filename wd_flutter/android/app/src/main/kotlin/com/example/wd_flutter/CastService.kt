package com.example.wd_flutter

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import android.view.Surface
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat

class CastService : Service() {
    private var projection: MediaProjection? = null
    private var vdisplay: VirtualDisplay? = null
    private var codec: MediaCodec? = null
    private var inputSurface: Surface? = null
    private val handler = Handler(Looper.getMainLooper())
    @Volatile private var running = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                val rc = intent.getIntExtra(EXTRA_RESULT_CODE, -1)
                val data = intent.getParcelableExtra<android.content.Intent>(EXTRA_RESULT_DATA)
                if (rc < 0 || data == null) {
                    stopSelf()
                    return START_NOT_STICKY
                }
                startForegroundWithNotification()
                try {
                    beginCapture(rc, data)
                } catch (e: Exception) {
                    Log.e(TAG, "beginCapture failed", e)
                    WdNative.stopCast()
                    stopSelf()
                }
            }
            ACTION_STOP -> {
                shutdown()
                stopSelf()
            }
        }
        return START_NOT_STICKY
    }

    private fun startForegroundWithNotification() {
        val nm = getSystemService(NotificationManager::class.java)
        if (android.os.Build.VERSION.SDK_INT >= 26) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "Screen casting", NotificationManager.IMPORTANCE_LOW)
            )
        }
        val n: Notification = if (android.os.Build.VERSION.SDK_INT >= 26) {
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Casting to WD receiver")
                .setSmallIcon(android.R.drawable.ic_menu_share)
                .setOngoing(true)
                .build()
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
                .setContentTitle("Casting to WD receiver")
                .setSmallIcon(android.R.drawable.ic_menu_share)
                .build()
        }
        if (android.os.Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIF_ID, n)
        }
    }

    private fun beginCapture(resultCode: Int, data: Intent) {
        val metrics = resources.displayMetrics
        var w = maxOf(metrics.widthPixels, metrics.heightPixels)
        var h = minOf(metrics.widthPixels, metrics.heightPixels)
        // Cap to 1080p-ish, keep even dims.
        val capW = 1280
        if (w > capW) { h = h * capW / w; w = capW }
        w -= w % 2; h -= h % 2
        val fps = 30
        val bitrateKbps = 6000

        val name = "Android Phone"
        val verdict = WdNative.startCast(
            CastPrefs.host, CastPrefs.port, name,
            w, h, fps, bitrateKbps,
        )
        Log.i(TAG, "startCast verdict: $verdict")
        val state = org.json.JSONObject(verdict).optString("state")
        if (state != "streaming") {
            val err = org.json.JSONObject(verdict).optString("error", "session rejected")
            WdNative.stopCast()
            CastBus.post(CastEvent.SessionRejected, err)
            stopSelf()
            return
        }

        val mpm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        val mp = mpm.getMediaProjection(resultCode, data)
        if (mp == null) {
            CastBus.post(CastEvent.SessionRejected, "media projection unavailable")
            stopSelf()
            return
        }
        projection = mp
        mp.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                shutdown(); stopSelf()
            }
        }, handler)

        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR)
        }

        codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC).also { enc ->
            enc.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = enc.createInputSurface()
            enc.start()
        }

        vdisplay = projection!!.createVirtualDisplay(
            "wd-cast", w, h, metrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            inputSurface, null, handler,
        )

        running = true
        Thread({ drainLoop() }, "wd-encoder-drain").start()
        CastBus.post(CastEvent.Streaming)
    }

    private fun drainLoop() {
        val ci = android.media.MediaCodec.BufferInfo()
        var lastKeyframeCheck = 0L
        while (running) {
            val enc = codec ?: break
            // Honor receiver keyframe requests every ~200ms.
            val now = SystemClockMillis()
            if (now - lastKeyframeCheck > 200) {
                lastKeyframeCheck = now
                if (WdNative.takeKeyframe()) {
                    try {
                        val p = android.os.Bundle().apply {
                            putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
                        }
                        enc.setParameters(p)
                        Log.i(TAG, "keyframe requested by receiver")
                    } catch (_: Exception) {}
                }
            }
            val idx = enc.dequeueOutputBuffer(ci, 10_000)
            if (idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                enc.outputFormat.getByteBuffer("csd-0")?.let { csd ->
                    val b = ByteArray(csd.remaining()); csd.get(b)
                    WdNative.sendVideo(b, true, ptsUs(ci.presentationTimeUs))
                }
                continue
            }
            if (idx < 0) continue
            val buf = enc.getOutputBuffer(idx) ?: continue
            if (ci.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                val b = ByteArray(buf.remaining()); buf.get(b)
                WdNative.sendVideo(b, true, ptsUs(ci.presentationTimeUs))
                enc.releaseOutputBuffer(idx, false)
                continue
            }
            if (ci.size > 0) {
                val b = ByteArray(ci.size)
                buf.position(ci.offset); buf.limit(ci.offset + ci.size)
                buf.get(b)
                WdNative.sendVideo(b, false, ptsUs(ci.presentationTimeUs))
            }
            enc.releaseOutputBuffer(idx, false)
        }
    }

    private fun ptsUs(v: Long): Long = if (v > 0) v else SystemClockMicros()

    private fun SystemClockMillis(): Long =
        android.os.SystemClock.uptimeMillis()

    private fun SystemClockMicros(): Long =
        android.os.SystemClock.uptimeMillis() * 1000L

    private fun shutdown() {
        running = false
        try { vdisplay?.release() } catch (_: Exception) {}
        try { inputSurface?.release() } catch (_: Exception) {}
        try {
            codec?.let { it.stop(); it.release() }
        } catch (_: Exception) {}
        codec = null
        try { projection?.stop() } catch (_: Exception) {}
        projection = null
        WdNative.stopCast()
    }

    companion object {
        private const val TAG = "WDCast"
        private const val CHANNEL_ID = "wd_cast"
        private const val NOTIF_ID = 42
        const val ACTION_START = "com.example.wd_flutter.START"
        const val ACTION_STOP = "com.example.wd_flutter.STOP"
        const val EXTRA_RESULT_CODE = "rc"
        const val EXTRA_RESULT_DATA = "data"

        fun start(ctx: Context, resultCode: Int, data: Intent) {
            val i = Intent(ctx, CastService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_RESULT_CODE, resultCode)
                putExtra(EXTRA_RESULT_DATA, data)
            }
            if (android.os.Build.VERSION.SDK_INT >= 26) ctx.startForegroundService(i)
            else ctx.startService(i)
        }

        fun stop(ctx: Context) {
            ctx.startService(Intent(ctx, CastService::class.java).apply { action = ACTION_STOP })
        }
    }
}
