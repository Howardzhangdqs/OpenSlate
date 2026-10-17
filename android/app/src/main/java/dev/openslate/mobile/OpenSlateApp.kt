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
