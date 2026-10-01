pluginManagement {
    repositories {
        // CN 可达镜像（dl.google.com / repo.maven.org 直连不可达；且
        // Gradle JVM 不认 http_proxy 环境变量，直连仓库会挂死）。
        // aliyun 三仓完整镜像 google/central/gradle-plugin，独占即可。
        maven("https://maven.aliyun.com/repository/gradle-plugin")
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/public")
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/public")
    }
}

rootProject.name = "OpenSlateMobile"
include(":app")
