# Ferrite E2E

> **[English Documentation (README.md)](README.md)**

**Ferrite E2E**, harici ağır tarayıcı sunucuları yerine doğrudan **Chrome Manifest V3 (`chrome.debugger`) eklentisi + Chrome Native Messaging + yerel Rust/Tokio motoru** ile çalışan hızlı ve hibrit bir uçtan uca (E2E) test otomasyon aracıdır.

Testlerinizi gerçek Chrome tarayıcınızda; jQuery tarzı `.ferrite` dili, zorunlu noktalı virgül (`;`) ve `.expect()` disiplini, servis mock'lama, konsol hata yakalama, görsel regresyon (`pixelmatch`), canlı akış kaydı ve yapay zeka ajanları için yerleşik **MCP** desteğiyle yazıp çalıştırabilirsiniz.

---

## Kurulum ve Hızlı Başlangıç

### 1. Chrome Eklentisini Kurun
**Ferrite E2E Bridge** eklentisini Chrome Web Store'dan yükleyin *(veya [GitHub Releases](../../releases) sayfasından `ferrite-e2e-chrome-extension.zip` dosyasını indirip açın, `chrome://extensions` adresinde **Geliştirici modu**'nu açıp **Paketlenmemiş öğe yükle** ile yükleyin)*.

### 2. Projenize CLI Paketini Ekleyin
```bash
npm install -D @halil07/ferrite
```

### 3. Chrome ile Ferrite'ı Bağlayın (Tek Seferlik)
`chrome://extensions` sayfasından eklentinin 32 karakterlik **Kimlik (Extension ID)** değerini kopyalayın ve Native Messaging kaydını yapın:

```bash
npx ferrite setup --extension-id <EXTENSION_ID>
```
*(Desteklenen tarayıcılar: `--browser chrome` (varsayılan), `edge`, `brave`, `chromium`.)*

`chrome://extensions` üzerinden eklentiyi yenileyin (↻). Eklenti ikonuna tıkladığınızda **Native Host: Connected** görünmelidir.

### 4. (İsteğe Bağlı) VS Code Eklentisini Kurun
`.ferrite` sözdizimi renklendirmesi, snippet'ler ve bağlam-duyarlı otomatik tamamlama (`.expect().visible()`, `.expect().count()`, seçiciler ve kullanıcı makroları) için:
- Releases sayfasındaki `ferrite-e2e-vscode-extension.zip` paketini kurun (veya depoyu klonladıysanız `.\scripts\install-vscode-ext.ps1` çalıştırın) ve VS Code'da `Ctrl+Shift+P` → `Developer: Reload Window` yapın.

### 5. Proje Oluşturun ve Testleri Çalıştırın
```bash
npx ferrite init
npx ferrite check
npx ferrite run
```

---

## `.ferrite` Dili ile Test Yazma

`.ferrite`, önce seçip sonra işlem yapan jQuery tarzı okunabilir bir programlama sözdizimine sahiptir. Her ifade `;` ile biter, eylemler seçiciye zincirlenir ve doğrulamalar `.expect()` altında yazılır.

### Suite Yapısı, Değişkenler, Import ve Makrolar

```ferrite
suite("odeme-akisi");
import "./helpers/auth.ferrite";

let base_url = "https://example.com";

beforeEach {
  await goto("about:blank");
  await goto("${base_url}", "load", 15s);
}

macro login(email, password) {
  await $("#email").fill("${email}");
  await $("#password").fill("${password}").press("Enter");
}

test("kullanici-giris-yapabilir") {
  await login("ada@example.com", "gizli-sifre");
  await $text("Hoş geldin, Ada").expect().visible();
}
```

### Seçiciler: `$`, `$text`, `$role` ve `@ref`

| Seçici | Ne Zaman Kullanılır | Örnek |
| :--- | :--- | :--- |
| `$("css")` | CSS seçicileri (`querySelectorAll`), `:eq(n)` ve `:first` destekli | `await $(".todo-list li label:eq(0)").dblclick();` |
| `$text("...")` | Accessibility Tree üzerindeki görünür metin | `await $text("1 item left").expect().visible();` |
| `$role("role", "name"?)` | ARIA rolü ve erişilebilir isim | `await $role("button", "Giriş Yap").exact().click();` |
| `$(@e1)` / `@e1` | `await snapshot("interactive");` çıktısındaki deterministik ref | `await $(@e1).fill("ada@example.com");` |

> **İpucu:** Tek tırnak (`'...'`) da desteklenir; özellikle attribute seçicilerde kaçış karakteri yazmadan temiz kullanım sağlar:  
> `await $('.filters a[href="#/active"]').click();`

### Eylemler ve Zincirleme (Chaining)

Önce elemanı seçin, ardından tek veya zincirleme eylemler çağırın:

```ferrite
// Tıklama, çift tıklama, üzerine gelme
await $("#submit-btn").click();
await $(".todo-list li label:eq(0)").dblclick();
await $(".todo-list li:first").hover();

// Alan doldurma (.fill / .val) + klavye zincirleme
await $("input.new-todo").fill("Süt al").press("Enter");
await $("input.search").val("Ferrite").press("Enter");

// Klavye kısayolları ve metin yazma
await $(".todo-list li label:eq(0)")
  .dblclick()
  .press("Control+a")
  .press("Backspace")
  .type("Yulaf sütü al")
  .press("Enter");

// İndeks ve zaman aşımı niteleyicileri (eylemden veya .expect() öncesi)
await $(".item").eq(2).timeout(10s).click();
await $role("button", "Kaydet").exact().click(8s);
```

### Doğrulamalar (`.expect()`)

Tüm görünürlük ve sayı doğrulamaları `.expect()` ile yazılır (varsayılan `5s` otomatik bekleme uygular; isterseniz özel süre verebilirsiniz):

```ferrite
await $("header.header h1").expect().visible();
await $("input.new-todo").expect().visible(10s);
await $(".footer").expect().hidden();
await $(".todo-list li").expect().count(3);
await $(".todo-list li.completed").expect().count(1, 5s);

await $text("Süt al").exact().expect().visible();
await $text("Eski görev").expect().hidden();
```

### Sayfa Yönlendirme, Bekleme ve Görsel Regresyon

```ferrite
await goto("https://example.com");
await goto("https://example.com", "domcontentloaded", 10s);
await reload();       // önbelleği atlamak için: await reload(true);
await sleep(1s);      // ms, s, m birimleri desteklenir (ör. 500ms, 5s)

// Görsel regresyon (anti-aliasing toleranslı pixelmatch karşılaştırması)
await screenshot("ana-sayfa", 0.1, 0.005);
```
Arayüz bilinçli olarak değiştiğinde baseline PNG'leri güncellemek için:
```bash
npx ferrite run --update-baselines
```

### Servis İsteklerini Manipüle Etme (`mock.when` ve `mock.load`)

Tarayıcıdan çıkan HTTP/XHR/Fetch isteklerini `:param` yol parametresi, `params` ve JSON `data` alt küme (subset) eşleşmesi, gecikme (`delay`), ağ hatası simülasyonu ve **saf response JSON dosyalarıyla** manipüle edin:

```ferrite
// Akıcı (fluent) kural (`:id` değeri `params`'tan alınır; kalanlar query alt kümesi olarak eşleşir)
await mock.when("GET", "/api/users/:id/orders")
  .params({ id: "42", page: "1" })
  .reply(200, "./mocks/responses/orders.json")
  .delay(300ms);

// POST gövdesi alt küme eşleşmesi (timestamp vb. ek alanlar yok sayılır)
await mock.when("POST", "/api/login")
  .data({ email: "admin@test.com" })
  .header("x-mock", "true")
  .reply(200, "./mocks/responses/login-ok.json");

// Ağ zaman aşımı veya kopma simülasyonu
await mock.when("GET", "/api/health").delay(200ms).fail("TimedOut");
await mock.when("GET", "/api/down").abort();

// Büyük projeler için harici senaryo dosyasından (`*.mock.json`) toplu yükleme
await mock.load("./mocks/checkout.mock.json");

// Sekmedeki tüm mock'ları temizle
await mock.clear();
```

### Konsol Kaydı ve Hata Yakalama (`console`)

```ferrite
await console.start();
await console.clear();

// ... test adımları ...

await console.expect("Uygulama hazır", "info", 5s);
await console.noErrors();
```

---

## Canlı Tarayıcı Akışı Kaydetme

Chrome'da yaptığınız tıklama ve yazma adımlarını otomatik olarak `.ferrite` test koduna dönüştürün (bitirmek için `Ctrl+C`):

```bash
npx ferrite record --url https://todomvc.com/examples/react/dist/ --out tests/recorded.ferrite
```

---

## CLI Komutları

| Komut | Açıklama |
| :--- | :--- |
| `npx ferrite init [klasör]` | Yeni `.ferrite` projesi oluşturur (`ferrite.toml`, helper, fixture, örnek test) |
| `npx ferrite check [yol]` | Tarayıcı açmadan `.ferrite` dosyalarını sözdizimi ve kural açısından doğrular |
| `npx ferrite compile [yol]` | `.ferrite` dosyalarını JSON `TestSuite` çıktısına derler |
| `npx ferrite run [yol]` | Testleri canlı Chrome'da çalıştırır (`--update-baselines`, `--json`, `--snapshots-dir`) |
| `npx ferrite record` | Canlı Chrome etkileşimlerini `.ferrite` dosyasına kaydeder |
| `npx ferrite setup --extension-id <ID>` | Chrome/Edge/Brave/Chromium Native Messaging kaydını yapar |
| `npx ferrite mcp` | Yapay zeka ajanları için stdio MCP (JSON-RPC 2.0) sunucusunu başlatır |

---

## Yapay Zeka Ajanı Entegrasyonu (`ferrite mcp`)

Ferrite; Claude Desktop, Cursor, Antigravity ve VS Code Copilot gibi yapay zeka istemcilerinin Chrome'da **attach edilmiş sekmelere** erişip JavaScript çalıştırabilmesi (`browser_evaluate`), Accessibility snapshot alabilmesi (`browser_snapshot`), servis mock'layabilmesi (`browser_mock`), ekran görüntüsü görebilmesi (`browser_screenshot`) ve `.ferrite` testlerini çalıştırabilmesi (`browser_run_ferrite`) için yerleşik bir **MCP** sunucusu içerir.

İstemcinizin MCP ayarlarına ekleyin:

```json
{
  "mcpServers": {
    "ferrite": {
      "command": "npx",
      "args": ["-y", "@halil07/ferrite", "mcp"]
    }
  }
}
```

---

## Örnek Projeleri Çalıştırma

### TodoMVC Örneği
Depo içindeki [examples/todomvc](examples/todomvc) klasöründe `https://todomvc.com/examples/react/dist/` için hazırlanmış 5 suite / 11 test case'lik örnek proje bulunur:

```bash
npx ferrite check examples/todomvc
npx ferrite run examples/todomvc
```

### JSONPlaceholder Studio Örneği (Hibrit Mock Sistemi)
[examples/jsonplaceholder-studio](examples/jsonplaceholder-studio) klasöründe, resmi demo sitesinin ([halil07/ferrite-mock-example-site](https://github.com/halil07/ferrite-mock-example-site)) tüm rotalarını `mock.load` + `mock.when` ile mock'layarak test eden 5 suite / 8 test case'lik örnek proje bulunur:

```bash
npx ferrite check examples/jsonplaceholder-studio
npx ferrite run examples/jsonplaceholder-studio
```
