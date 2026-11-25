# Ferrite E2E Sistem Mimarisi ve Tasarım Notları

## 1. Uçtan Uca Veri Akışı

```
[E2E Test Runner (ferrite run)]
        |  WebSocket (127.0.0.1, Origin yasaklı, 256-bit Bearer Token)
        v
[ferrite-daemon (Tokio)] ---> [ferrite-locator / ferrite-visual / ferrite-cdp]
        |  tokio::sync::mpsc + oneshot korelasyonu + ChunkReassembler
        v
[ferrite-ipc (StdioBridge OS Pipe Threads)]
        |  4-byte LE uzunluk öneki + UTF-8 JSON (stdin / stdout)
        v
[Chrome Tarayıcısı Native Messaging Pipe]
        |  chrome.runtime.connectNative("com.ferrite.e2e")
        v
[MV3 Service Worker (extension/src/background.js)]
        |  chrome.debugger.sendCommand({ tabId, sessionId }, method, params)
        v
[Hedef Web Sekmesi / OOPIF]
```

## 2. Haberleşme Protokolü

Her iki yönde akan mesajlar `WireMessage` zarfını kullanır (`crates/ferrite-core/src/protocol.rs`, `extension/src/protocol.js`):

- **Request**: `{"v":1,"kind":"request","id":42,"method":"cdp.send","params":{"tabId":1,"method":"Page.navigate","params":{"url":"https://example.com"}}}`
- **Response**: `{"v":1,"kind":"response","id":42,"ok":true,"result":{"frameId":"..."}}`
- **Event**: `{"v":1,"kind":"event","event":"cdp.event","payload":{"tabId":1,"method":"Page.loadEventFired","params":{}}}`
- **Chunk**: `{"v":1,"kind":"chunk","transfer_id":"tx-1","index":0,"total":3,"data":"<base64>"}`

### 1 MiB Native Messaging Sınırı ve Chunking
Chrome, Native Host'tan (`stdout`) eklentiye giden tek bir mesajı **kesin olarak 1 MiB** ile sınırlar (`MAX_HOST_TO_EXT_BYTES`). Eklentiden host'a sınır 64 MiB olsa da tam ekran PNG veya büyük `Accessibility.getFullAXTree` yanıtları simetrik chunking ile `512 KiB` ham JSON parçalarına (`~683 KiB` base64) bölünür.

## 3. `chrome.debugger` Yaşam Döngüsü ve Stealth
- `DebuggerHub.attach(tabId)` (`extension/src/debugger-hub.js`) eşzamanlı çağrıları `pending` haritası ile tekilleştirir ve `chrome://` / `devtools://` gibi yasaklı şemaları reddeder.
- Arka plan sekmelerinde `requestAnimationFrame` ve `setTimeout` kısıtlamasını engellemek için `Emulation.setFocusEmulationEnabled({ enabled: true })` uygulanır.
- MV3 Service Worker uyuyup yeniden başlatıldığında `chrome.storage.session` üzerindeki sekme kayıtları okunur; sahiplenme doğrulaması `Runtime.enable` yerine `Page.getFrameTree` ile yapılarak sayfa içi anti-bot dedektörlerinin `Runtime.enable` yan etkilerini yakalaması önlenir.

## 4. Kritik Güvenlik ve Performans Darboğazları

### Güvenlik
1. **Native Messaging Manifest (`allowed_origins`)**: Yalnızca kesin `chrome-extension://<32-char-id>/` eşleşmesi verilmelidir; wildcard yasaktır.
2. **CSWSH (Cross-Site WebSocket Hijacking)**: Kötü niyetli bir web sayfası `ws://127.0.0.1:<port>` adresine bağlanmaya çalışırsa tarayıcı otomatik olarak `Origin` başlığı ekler. `WsServer` (`crates/ferrite-daemon/src/ws_server.rs`) `Origin` başlığı içeren tüm el sıkışmaları `403 Forbidden` ile reddeder ve yerel istemcilerden 256-bit `getrandom` Bearer token ister.
3. **`stdout` Kirlenmesi**: Native Messaging modunda `stdout` ikili çerçeveleme borusudur. Tek bir `println!` bile 4-byte LE uzunluk başlığını bozar; tüm loglar `stderr` üzerine yönlendirilir.
4. **PNG Decompression Bomb ve Path Traversal**: `ferrite-visual` piksel tamponu ayırmadan önce PNG IHDR `width * height` çarpımını `16 MP` ile sınırlar ve snapshot adlarında `..`, `/` veya `\` karakterlerini reddeder.

### Performans
1. **Pipe I/O ve Tokio Executor**: Windows anonim pipe'ları bloklayan `ReadFile`/`WriteFile` davranışına sahiptir. `StdioBridge` (`crates/ferrite-ipc/src/stdio_bridge.rs`) okuma ve yazmayı iki ayrı OS thread'ine izole ederek Tokio runtime'ını bloklamaz.
2. **MV3 Service Worker Uyku Modu**: Chrome 30 saniyelik hareketsizlikte Service Worker'ı askıya alabilir. `chrome.alarms` ile 30 saniyelik kalp atışı + kesinti durumunda `DebuggerHub.restore()` ve üstel geri çekilmeli (`exponential backoff`) yeniden bağlanma uygulanır.
3. **Büyük AX Ağaçları ve CDP Olay Fırtınası**: `Network.enable` veya sık DOM mutasyonları yüksek hacimli `cdp.event` akışı üretir. `NativeBridgeClient` `1024` kapasiteli `broadcast` ve `256` kapasiteli `mpsc` kanallarıyla geri basıncı (backpressure) sınırlar; `ferrite-locator` ise `ignored` ve etiketsiz `generic` sarmalayıcıları ağaç kurulumunda budar.
