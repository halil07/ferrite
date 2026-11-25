# Ferrite E2E

Microsoft Playwright benzeri ancak harici bir Node.js süreci yerine **Chrome Manifest V3 (`chrome.debugger`) + Native Messaging + Tokio Rust arka plan süreci** kullanan hibrit E2E test otomasyon motoru.

Testlerinizi proje yapısında okunabilir **`.ferrite` DSL dosyaları** ile yazabilir; makrolar (`macro`/`use`), `import`, `beforeEach`, dosya fixture'ları (`body file`) ve görsel regresyon (`pixelmatch`) kullanabilirsiniz.

---

## Bileşenler

- **`.ferrite` DSL & Proje Derleyicisi (`crates/ferrite-dsl`)**: Lexer, recursive-descent parser, `${var}` interpolasyonu, `import`, `beforeEach`, `macro`/`use` genişletmesi ve `ferrite.toml` okuyucusu.
- **MV3 Chrome Eklentisi (`extension/`)**: `chrome.debugger` API'si ile aktif/yeni sekmelere bağlanır ve `chrome.runtime.connectNative("com.ferrite.e2e")` üzerinden Rust süreciyle çift yönlü CDP proxy'si olarak çalışır.
- **Native Messaging Köprüsü (`crates/ferrite-ipc`)**: 4-byte little-endian framing, 1 MiB sınırını aşan payload'lar için otomatik `512 KiB` base64 chunking/reassembly ve Tokio çoklu talep korelasyonu.
- **CDP Domain Yönetimi (`crates/ferrite-cdp`)**: `Page`, `Target` (`autoAttach`) ve ref-counted `Network` + `Fetch` intercept domain'leri.
- **Deterministik Locator Engine (`crates/ferrite-locator`)**: `Accessibility.getFullAXTree` üzerinden CSS'den bağımsız parmak iziyle kalıcı `@e1`, `@e2` referansları, `hover`, `dblclick`, `press`, `type` ve *self-healing*.
- **Görsel Regresyon (`crates/ferrite-visual`)**: PNG decompression bomb koruması, YIQ renk farkı + 3x3 anti-aliasing dışlama (`pixelmatch`) + maskeleme.
- **NPM Paketi (`npm/ferrite-e2e/`) & VS Code Eklentisi (`vscode-ferrite/`)**: `npx ferrite` başlatıcısı ve `.ferrite` sözdizimi renklendirmesi + kod parçacıkları (snippets).

---

## TodoMVC E2E Testlerini Çalıştırma Rehberi (`examples/todomvc`)

[examples/todomvc](examples/todomvc) klasörü, canlı `https://todomvc.com/examples/react/dist/` uygulaması için hazırlanmış 5 suite / 11 test case'lik eksiksiz bir E2E projesidir.

### Adım 1: Rust Binary'yi Derleyin
Proje kök dizininde testleri doğrulayın ve release binary'yi oluşturun:

```powershell
cargo test --workspace
cargo build --release -p ferrite-cli
```

*(Eğer Chrome arka planda eski bir `ferrite.exe` sürecini açık tutuyorsa derlemeden önce `Stop-Process -Name ferrite -Force -ErrorAction SilentlyContinue` çalıştırın.)*

### Adım 2: Chrome Eklentisini Yükleyin ve Native Messaging Kaydını Yapın
1. Chrome'da `chrome://extensions` adresini açın.
2. Sağ üstten **Geliştirici modu**'nu açın.
3. **Paketlenmemiş öğe yükle** butonuna tıklayıp repo içindeki `extension/` klasörünü seçin.
4. Yüklenen **Ferrite E2E Bridge** kartındaki 32 karakterlik **Kimlik (Extension ID)** değerini kopyalayın.
5. Native Messaging host kaydını yapın:

```powershell
.\target\release\ferrite.exe setup --extension-id <EXTENSION_ID>
```

6. `chrome://extensions` sayfasında eklentinin **Yenile (↻)** simgesine tıklayın. Eklenti popup'ına tıkladığınızda **Native Host: Connected** görünmelidir.

### Adım 3: (İsteğe Bağlı) VS Code `.ferrite` Renklendirmesini Kurun

```powershell
.\scripts\install-vscode-ext.ps1
```

Kurulumdan sonra VS Code'da `Ctrl+Shift+P` -> `Developer: Reload Window` çalıştırın.

### Adım 4: TodoMVC Testlerini Doğrulayın ve Çalıştırın

```powershell
# 1. Tüm .ferrite dosyalarını tarayıcı açmadan sözdizimi/derleme açısından kontrol et
.\target\release\ferrite.exe check .\examples\todomvc

# 2. (İsteğe bağlı) .ferrite dosyalarının derlendiği JSON TestSuite çıktısını incele
.\target\release\ferrite.exe compile .\examples\todomvc

# 3. Tüm TodoMVC E2E testlerini (5 suite, 11 case) canlı Chrome'da çalıştır
.\target\release\ferrite.exe run .\examples\todomvc
```

### Tek Bir Suite Çalıştırma ve Görsel Baseline Güncelleme

Belirli bir senaryoyu tek başına çalıştırmak için:
```powershell
.\target\release\ferrite.exe run .\examples\todomvc\tests\03-edit-and-delete.ferrite
```

Görsel regresyon (`05-visual-regression.ferrite`) baseline PNG'lerini ilk kez oluşturduktan sonra arayüz bilinçli değiştiğinde baseline'ları güncellemek için:
```powershell
.\target\release\ferrite.exe run .\examples\todomvc\tests\05-visual-regression.ferrite --update-baselines
```

### TodoMVC Test Kapsamı
- `01-initial-and-create.ferrite`: Boş başlangıç durumu, tekli/çoklu todo ekleme (`1 item left` / `3 items left`), kısa giriş reddi ve boşluk temizleme.
- `02-complete-and-toggle-all.ferrite`: Tekil checkbox tamamlama/geri alma ve `Toggle All` (`checkbox nth 0`) ile toplu tamamlama.
- `03-edit-and-delete.ferrite`: Double-click + `Enter` ile düzenleme, `Escape` ile iptal, `hover` + silme butonuyla öğe silme.
- `04-filters-and-clear.ferrite`: `All` / `Active` / `Completed` filtre linkleri ve `Clear completed` butonu.
- `05-visual-regression.ferrite`: Boş ve karma liste durumlarının `pixelmatch` ekran görüntüsü doğrulaması.

---

## Yeni `.ferrite` Projesi Başlatma ve Canlı Test Kaydı

Kendi projenizde sıfırdan başlamak için:

```powershell
.\target\release\ferrite.exe init .\my-e2e-tests
.\target\release\ferrite.exe check .\my-e2e-tests
.\target\release\ferrite.exe run .\my-e2e-tests
```

Tarayıcıda yaptığınız tıklama ve yazma adımlarını otomatik olarak `.ferrite` test koduna dönüştürmek için (`Ctrl+C` ile kaydı durdurup üretir):

```powershell
.\target\release\ferrite.exe record --url https://todomvc.com/examples/react/dist/ --out .\recorded.ferrite
```

Testlerinizde yakalanmamış JS hatalarını veya `console.error` çıktılarını yakalamak için `console start` ve `expect noConsoleErrors` / `expect console "..."` ifadelerini kullanabilirsiniz. Profil güvenliği için yıkıcı `Network.clearBrowserCookies`, `Network.clearBrowserCache` ve `Browser.close` CDP komutları hem eklenti hem daemon katmanında engellenir.

## GitHub Release ve Paketleme

- `.github/workflows/release.yml`: `v*` etiketi push edildiğinde (veya manuel `workflow_dispatch` ile) tüm testleri çalıştırır; Linux (`x64`/`arm64`), macOS (`x64`/`arm64`), Windows (`x64`) release binary'lerini, Chrome MV3 eklenti zip'ini ve VS Code eklenti zip'ini derleyip GitHub Release olarak yayınlar.
- Yerel Chrome Web Store paketi oluşturmak için: `.\scripts\package-extension.ps1`.

Ayrıntılı mimari, protokol akışı, güvenlik ve performans analizi için [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) dosyasına bakın.
