package com.example.wd_flutter

import android.content.Context

object WdNative {
    init {
        System.loadLibrary("wd_phone_core")
    }

    fun init(ctx: Context): Boolean =
        nativeInit(ctx.filesDir.absolutePath)

    fun hasToken(): Boolean = nativeHasToken()

    fun pair(host: String, port: Int, code: String, name: String): String =
        nativePair(host, port, code, name)

    fun startCast(
        host: String,
        port: Int,
        name: String,
        width: Int,
        height: Int,
        fps: Int,
        bitrateKbps: Int,
    ): String = nativeStartCast(host, port, name, width, height, fps, bitrateKbps)

    fun sendVideo(data: ByteArray, isConfig: Boolean, ptsUs: Long): Boolean =
        nativeSendVideo(data, isConfig, ptsUs)

    fun status(): String = nativeStatus()

    fun takeKeyframe(): Boolean = nativeTakeKeyframe()

    fun stopCast() = nativeStopCast()

    private external fun nativeInit(storeDir: String): Boolean
    private external fun nativeHasToken(): Boolean
    private external fun nativePair(host: String, port: Int, code: String, name: String): String
    private external fun nativeStartCast(
        host: String,
        port: Int,
        name: String,
        width: Int,
        height: Int,
        fps: Int,
        bitrateKbps: Int,
    ): String
    private external fun nativeSendVideo(data: ByteArray, isConfig: Boolean, ptsUs: Long): Boolean
    private external fun nativeStatus(): String
    private external fun nativeTakeKeyframe(): Boolean
    private external fun nativeStopCast()
}
