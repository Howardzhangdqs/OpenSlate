package dev.openslate.mobile

import android.app.Application
import android.content.Context
import coil3.ImageLoader
import coil3.SingletonImageLoader
import coil3.svg.SvgDecoder

class OpenSlateApp : Application(), SingletonImageLoader.Factory {
    override fun onCreate() {
        super.onCreate()
        instance = this
        // App 前后台跟踪（AgentNotifier）最早初始化点：必须早于任何 Activity /
        // Service 的 onCreate（RuntimeBridge.start 里的幂等调用晚于 AgentService
        // onCreate，不够早——AgentService 首次「前台撤 1001」判定依赖跟踪器就绪）。
        // 幂等 init（trackingRegistered 守卫），后续 RuntimeBridge.start 再调 no-op。
        dev.openslate.mobile.bridge.AgentNotifier.init(this)
    }

    /**
     * Coil3 全局 ImageLoader：显式注册 SvgDecoder（Markdown 内嵌 SVG）。
     * OkHttp 网络加载器无需手动注册——coil-network-okhttp 3.6 经
     * ServiceLoader 在 build() 时自动挂载（其手动注册 API 已 internal）。
     * 实现 SingletonImageLoader.Factory 是 Coil3 推荐的应用级注册方式。
     */
    override fun newImageLoader(context: Context): ImageLoader =
        ImageLoader.Builder(context)
            .components {
                add(SvgDecoder.Factory())
            }
            .build()

    companion object {
        lateinit var instance: OpenSlateApp
            private set
    }
}
