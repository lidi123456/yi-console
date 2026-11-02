package dev.yiconsole.app

// gen/android 是 `tauri android init` 生成的、不进仓库；构建前把这个文件拷到
// gen/android/app/src/main/java/dev/yiconsole/app/MainActivity.kt 覆盖生成的那份（CI 里有这一步）。

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.view.View
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  // 全面屏（edge-to-edge，targetSdk 35 起强制）下系统不再因为输入法缩小窗口，
  // 输入框会被键盘盖住。这里自己把内容区底部让出键盘的高度：
  // 只让出「键盘比导航栏高出的部分」，导航栏那一截网页里已经用 env(safe-area-inset-bottom) 留了。
  override fun onWebViewCreate(webView: WebView) {
    val root = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
      val ime = insets.getInsets(WindowInsetsCompat.Type.ime()).bottom
      val nav = insets.getInsets(WindowInsetsCompat.Type.navigationBars()).bottom
      v.setPadding(0, 0, 0, maxOf(0, ime - nav))
      insets
    }

    // 「用其他浏览器打开」：弹系统的应用选择框，不管有没有设默认浏览器都能挑（PortsView.tsx 的 openUrlExternal）。
    // 只接受 http / https，页面里的 iframe 也能调到这个接口，别的 scheme 一律不理
    webView.addJavascriptInterface(object {
      @JavascriptInterface
      fun openExternal(url: String) {
        val uri = Uri.parse(url)
        if (uri.scheme != "http" && uri.scheme != "https") return
        runOnUiThread {
          val view = Intent(Intent.ACTION_VIEW, uri).addCategory(Intent.CATEGORY_BROWSABLE)
          val chooser = Intent.createChooser(view, "用其他浏览器打开").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
          try { startActivity(chooser) } catch (_: Exception) {}
        }
      }
    }, "YiConsoleAndroid")

    // 返回键 / 返回手势交给网页（back.ts）：关掉弹窗、面板，或回到上一个页面；
    // 退无可退时网页先提示「再按一次退出」，2 秒内再按（或页面还没加载好）才退出。
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        webView.evaluateJavascript("(window.__yiConsoleBack ? window.__yiConsoleBack() : false)") { r ->
          if (r != "true") finish()
        }
      }
    })
  }
}
