// オンボーディングの「次へ」操作。
//
// WebView2 の初期化スクリプトとして読み込まれるため、DOM の構築完了を
// 待ってから要素を取得する。ページ側の CSP は script-src 'none' だが、
// ホストが注入する初期化スクリプトは CSP の対象外。
(function () {
  function boot() {
    var button = document.getElementById("startButton");
    if (!button) return;
    var sent = false;

    function finish() {
      if (sent) return;
      sent = true;
      button.disabled = true;
      if (!window.ipc) {
        sent = false;
        button.disabled = false;
        return;
      }
      window.ipc.postMessage(JSON.stringify({ cmd: "get_started" }));
    }

    button.addEventListener("click", finish);
    document.addEventListener("keydown", function (event) {
      if (event.key === "Enter" && !event.repeat) finish();
    });
    button.focus();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot, { once: true });
  } else {
    boot();
  }
})();
