//! KCU Janelas: mostra a janela de UMA app do Windows numa pagina web e devolve-lhe rato,
//! teclado e toque (sem Ambiente de Trabalho Remoto, sem mostrar o resto do ecra).
//! Corre na sessao do Windows do utilizador (tarefa agendada "Interactive") e so escuta em
//! 127.0.0.1; o https vem de um proxy a frente (ex.: `tailscale serve --https=3095 http://127.0.0.1:3095`).
//! - Config: kcu-janelas.json ao lado do .exe, ou o ficheiro em KCU_JANELAS_CONFIG.
//! - So entram os IPs da lista `permitidos` e quem tiver a `chave` (vai no # do endereco, nunca no registo).
//! - /?app=<id> abre a pagina; /ws?app=<id>&t=<chave> abre a app (se preciso) e manda a janela
//!   (H.264 pela placa NVIDIA com ffmpeg, ou JPEG) e o som so dessa app (se a pagina o pedir).
#![windows_subsystem = "windows"]

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    ffi::c_void,
    hash::{Hash, Hasher},
    io::Write,
    sync::{atomic::{AtomicBool, AtomicIsize, Ordering}, Arc, Mutex},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
    System::Diagnostics::ToolHelp::*,
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

/// kcu-janelas.json: o ficheiro em KCU_JANELAS_CONFIG, senao ao lado do .exe.
fn caminho_config() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("KCU_JANELAS_CONFIG") {
        if !p.trim().is_empty() {
            return p.into();
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("kcu-janelas.json")))
        .unwrap_or_else(|| "kcu-janelas.json".into())
}

/// Pasta da config: o registo (kcu-janelas.log) e os icones (kcu-janelas-icones/) ficam la.
fn pasta() -> std::path::PathBuf {
    caminho_config().parent().map(|p| p.to_path_buf()).unwrap_or_default()
}

#[derive(Deserialize, Clone)]
struct App {
    nome: String,
    exe: String,
    processo: String,
    /// fechar a app quando ninguem a esta a ver (false para apps com trabalho a correr, ex.: terminais)
    #[serde(default = "sim")]
    fechar: bool,
}

fn sim() -> bool {
    true
}

#[derive(Deserialize)]
struct Config {
    porta: u16,
    /// ffmpeg com h264_nvenc (video H.264 pela placa NVIDIA); sem ele usa-se JPEG
    #[serde(default)]
    ffmpeg: Option<String>,
    /// IPs que podem entrar (o ultimo do X-Forwarded-For). Pedidos diretos em 127.0.0.1 entram sempre.
    #[serde(default)]
    permitidos: Vec<String>,
    apps: HashMap<String, App>,
    /// a chave (minimo 16 caracteres) ...
    #[serde(default)]
    chave: Option<String>,
    /// ... ou onde a ler: {"ficheiro": "segredos.json", "campo": "janelas"}
    #[serde(default)]
    segredos: Option<Segredos>,
    /// endereco do botao "Inicio" da pagina (ex.: o teu painel); sem ele o botao nao aparece
    #[serde(default)]
    inicio: Option<String>,
    /// microfone do aparelho -> app: nome (ou parte) do dispositivo de SAIDA de um cabo virtual
    /// (ex.: "CABLE Input" do VB-Audio); a app usa o lado de entrada ("CABLE Output") como microfone
    #[serde(default = "cabo_padrao")]
    microfone: String,
    /// camara do aparelho -> app, pela "OBS Virtual Camera" (o OBS so tem de estar instalado);
    /// false para nunca mostrar o botao
    #[serde(default = "sim")]
    camara: bool,
    /// /?app=ecra mostra o ecra inteiro (monitor principal); false para desligar
    #[serde(default = "sim")]
    ecra: bool,
}

/// id especial: o ecra inteiro do PC em vez de uma app
const ECRA: &str = "ecra";

impl Config {
    fn app(&self, id: &str) -> Option<App> {
        if id == ECRA && self.ecra {
            // processo vazio = ecra inteiro: nada para abrir nem fechar
            return Some(App { nome: "Ecrã do PC".into(), exe: String::new(), processo: String::new(), fechar: false });
        }
        self.apps.get(id).cloned()
    }
}

/// VB-CABLE: o lado de saida chama-se "CABLE Input" (ingles) ou "Altifalantes (VB-Audio Virtual
/// Cable)" (portugues); o nome do driver aparece em todos os idiomas.
fn cabo_padrao() -> String {
    "VB-Audio Virtual Cable".into()
}

#[derive(Deserialize)]
struct Segredos {
    ficheiro: String,
    campo: String,
}

struct Estado {
    cfg: Config,
    chave: String,
}

fn nota(t: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(pasta().join("kcu-janelas.log")) {
        let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{s} {t}");
    }
}

fn ler_json(p: &str) -> serde_json::Value {
    let t = std::fs::read_to_string(p).unwrap_or_default();
    serde_json::from_str(t.trim_start_matches('\u{feff}')).unwrap_or(serde_json::Value::Null)
}

#[tokio::main]
async fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // manter o MTA do COM vivo o tempo todo: os fios de som/microfone entram e saem dele e,
        // quando o ultimo saia, a captura de imagem (WinRT) da sessao seguinte caia (0xc0000005)
        let _ = windows::Win32::System::Com::CoIncrementMTAUsage();
    }
    std::panic::set_hook(Box::new(|i| {
        nota(&format!("PANICO: {i} | {}", std::backtrace::Backtrace::force_capture()).replace('\n', " / "));
    }));
    // `--guarda [config]`: tarefa SYSTEM no arranque; o caminho da config passa ao ajudante pelo ambiente
    let args: Vec<String> = std::env::args().collect();
    let modo = args.get(1).cloned().unwrap_or_default();
    if modo == "--guarda" {
        if let Some(c) = args.get(2) {
            std::env::set_var("KCU_JANELAS_CONFIG", c);
        }
        guarda();
        return;
    }
    let cfg: Config = match serde_json::from_value(ler_json(&caminho_config().to_string_lossy())) {
        Ok(c) => c,
        Err(err) => {
            nota(&format!("config invalida ou em falta ({}): {err}; a sair", caminho_config().display()));
            return;
        }
    };
    let chave = match (&cfg.chave, &cfg.segredos) {
        (Some(c), _) => c.clone(),
        (None, Some(s)) => ler_json(&s.ficheiro)[s.campo.as_str()].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    };
    if chave.len() < 16 {
        nota("sem chave (minimo 16 caracteres) na config; a sair");
        return;
    }
    if modo == "--seguro" {
        ajudante(chave);
        return;
    }
    let _ = CHAVE.set(chave.clone());
    let porta = cfg.porta;
    let estado = Arc::new(Estado { cfg, chave });
    let app = Router::new()
        .route("/", get(pagina))
        .route("/ws", get(ws))
        .route("/icone", get(icone))
        .route("/sair", post(sair))
        .route("/enviar", post(receber_ficheiro).layer(axum::extract::DefaultBodyLimit::disable()))
        .route("/lista", get(lista))
        .route("/pedir", post(pedir))
        .route("/descarregar", get(descarregar))
        .with_state(estado);
    let l = tokio::net::TcpListener::bind(("127.0.0.1", porta)).await.expect("porta ocupada");
    nota(&format!("a escutar em 127.0.0.1:{porta}"));
    fechar_executar();
    axum::serve(l, app).await.unwrap();
}

/// IP real do aparelho: o ultimo do X-Forwarded-For que o `tailscale serve` acrescenta.
fn ip_de(h: &HeaderMap) -> String {
    h.get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').last())
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "local".into())
}

fn autorizado(e: &Estado, h: &HeaderMap) -> bool {
    let ip = ip_de(h);
    ip == "local" || e.cfg.permitidos.iter().any(|p| *p == ip)
}

#[derive(Deserialize)]
struct Q {
    app: Option<String>,
    v: Option<String>,
    b: Option<u32>,
    t: Option<String>,
    w: Option<u32>,
    q: Option<u8>,
}

async fn pagina(State(e): State<Arc<Estado>>, h: HeaderMap, Query(q): Query<Q>) -> Response {
    if !autorizado(&e, &h) {
        nota(&format!("403 pagina {}", ip_de(&h)));
        return (StatusCode::FORBIDDEN, "Este aparelho nao esta autorizado.").into_response();
    }
    let id = q.app.unwrap_or_default();
    let Some(a) = e.cfg.app(&id) else {
        return (StatusCode::NOT_FOUND, "App desconhecida.").into_response();
    };
    let html = PAGINA
        .replace("__NOME__", &a.nome)
        .replace("__ID__", &id) // id ja confirmado na config
        .replace("__INICIO__", &serde_json::to_string(&e.cfg.inicio).unwrap())
        .replace("__APP__", &serde_json::to_string(&id).unwrap());
    let mut r = Html(html).into_response();
    r.headers_mut().insert("cache-control", "no-store".parse().unwrap());
    r.headers_mut().insert("referrer-policy", "no-referrer".parse().unwrap());
    r
}

/// Icone real da app: kcu-janelas-icones/<id>.png ao lado da config (extrair-icones.ps1 tira-os dos .exe).
/// So ids da config: nunca um caminho vindo do pedido.
async fn icone(State(e): State<Arc<Estado>>, h: HeaderMap, Query(q): Query<Q>) -> Response {
    if !autorizado(&e, &h) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let id = q.app.unwrap_or_default();
    if e.cfg.app(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match std::fs::read(pasta().join("kcu-janelas-icones").join(format!("{id}.png"))) {
        Ok(b) => {
            let mut r = b.into_response();
            r.headers_mut().insert("content-type", "image/png".parse().unwrap());
            r.headers_mut().insert("cache-control", "max-age=86400".parse().unwrap());
            r
        }
        Err(_) if id == ECRA => {
            let mut r = ICONE_ECRA.into_response();
            r.headers_mut().insert("content-type", "image/svg+xml".parse().unwrap());
            r.headers_mut().insert("cache-control", "max-age=86400".parse().unwrap());
            r
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws(State(e): State<Arc<Estado>>, h: HeaderMap, Query(q): Query<Q>, up: WebSocketUpgrade) -> Response {
    let ip = ip_de(&h);
    if !autorizado(&e, &h) || q.t.as_deref() != Some(e.chave.as_str()) {
        nota(&format!("403 ws {ip}"));
        return StatusCode::FORBIDDEN.into_response();
    }
    let id = q.app.clone().unwrap_or_default();
    let Some(a) = e.cfg.app(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    nota(&format!("{ip} abriu {id}"));
    let largura = q.w.unwrap_or(0).min(8000);
    let qualidade = q.q.unwrap_or(70).clamp(20, 95);
    // video H.264 (NVENC) se o browser o pedir (WebCodecs) e houver ffmpeg; senao JPEG
    let ffmpeg = e.cfg.ffmpeg.clone().filter(|f| std::path::Path::new(f).exists());
    let video = if q.v.as_deref() == Some("h264") { ffmpeg.map(|f| (f, q.b.unwrap_or(12000).clamp(1500, 40000))) } else { None };
    let cabo = e.cfg.microfone.clone();
    let cam = e.cfg.camara;
    up.on_upgrade(move |s| sessao(s, a, largura, qualidade, video, cabo, cam))
}

// ---------------------------------------------------------------- janelas

fn hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

/// Caminho completo do programa de um processo.
fn caminho_de(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::*;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut n);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// `processo` pode ser so o nome ("Obsidian.exe") ou o caminho completo do programa; com o
/// caminho, so contam os processos desse programa exato.
fn pids_de(processo: &str) -> Vec<u32> {
    if processo.contains('\\') {
        let nome = std::path::Path::new(processo).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        return pids_por_nome(&nome)
            .into_iter()
            .filter(|p| caminho_de(*p).map(|c| c.eq_ignore_ascii_case(processo)).unwrap_or(false))
            .collect();
    }
    pids_por_nome(processo)
}

fn pids_por_nome(processo: &str) -> Vec<u32> {
    let mut v = Vec::new();
    unsafe {
        let Ok(s) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return v };
        let mut pe = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(s, &mut pe).is_ok() {
            loop {
                let n = String::from_utf16_lossy(&pe.szExeFile[..pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(0)]);
                if n.eq_ignore_ascii_case(processo) {
                    v.push(pe.th32ProcessID);
                }
                if Process32NextW(s, &mut pe).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(s);
    }
    v
}

// ---------------------------------------------------------------- login pelo browser
// Algumas apps abrem o browser do PC para o login (Google, etc.). Quando uma janela de browser vem
// para a frente durante a sessao, a pagina passa a mostra-la; quando a app volta para a frente
// (login feito) ou a pagina fecha, as janelas de browser abertas durante a sessao sao fechadas.

const BROWSERS: [&str; 7] = ["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe", "arc.exe"];

fn pids_browsers() -> Vec<u32> {
    BROWSERS.iter().flat_map(|n| pids_de(n)).collect()
}

fn janelas_de_pids(pids: Vec<u32>) -> Vec<isize> {
    let mut b = (pids, Vec::<isize>::new());
    if b.0.is_empty() {
        return Vec::new();
    }
    unsafe {
        let _ = EnumWindows(Some(junta), LPARAM(&mut b as *mut _ as isize));
    }
    b.1
}

fn pid_da_janela(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    pid
}

fn fechar_janela(h: isize) {
    unsafe {
        if IsWindow(hwnd(h)).as_bool() {
            let _ = PostMessageW(hwnd(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

struct Busca {
    pids: Vec<u32>,
    melhor: isize,
    area: i64,
}

unsafe extern "system" fn cada(h: HWND, l: LPARAM) -> BOOL {
    let b = &mut *(l.0 as *mut Busca);
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if !b.pids.contains(&pid) || !IsWindowVisible(h).as_bool() {
        return TRUE;
    }
    if GetWindow(h, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false) || GetWindowTextLengthW(h) == 0 {
        return TRUE;
    }
    let mut r = RECT::default();
    if GetWindowRect(h, &mut r).is_ok() {
        let a = ((r.right - r.left) as i64) * ((r.bottom - r.top) as i64);
        if a > b.area || IsIconic(h).as_bool() && b.melhor == 0 {
            b.area = a;
            b.melhor = h.0 as isize;
        }
    }
    TRUE
}

unsafe extern "system" fn diag(h: HWND, l: LPARAM) -> BOOL {
    let b = &mut *(l.0 as *mut (Vec<u32>, Vec<String>));
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if b.0.contains(&pid) {
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let mut t = [0u16; 80];
        let n = GetWindowTextW(h, &mut t);
        let mut c = [0u16; 80];
        let nc = GetClassNameW(h, &mut c);
        let dono = GetWindow(h, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false);
        b.1.push(format!(
            "pid={pid} vis={} dono={dono} titulo='{}' classe='{}' {}x{}",
            IsWindowVisible(h).as_bool(),
            String::from_utf16_lossy(&t[..n.max(0) as usize]),
            String::from_utf16_lossy(&c[..nc.max(0) as usize]),
            r.right - r.left,
            r.bottom - r.top
        ));
    }
    TRUE
}

/// Escreve no registo as janelas dos processos da app (quando nenhuma serve).
fn diagnostico(processo: &str) {
    let mut b = (pids_de(processo), Vec::<String>::new());
    unsafe {
        let _ = EnumWindows(Some(diag), LPARAM(&mut b as *mut _ as isize));
    }
    nota(&format!("diag {processo}: pids {:?}, {} janelas", b.0, b.1.len()));
    for l in b.1.iter().take(15) {
        nota(&format!("  {l}"));
    }
}

/// Como `cada`, mas so para janelas principais INVISIVEIS (app escondida na bandeja).
/// Ignora janelas de servico (IME, icone da bandeja, energia, crashpad...) e as pequenas.
unsafe extern "system" fn cada_escondida(h: HWND, l: LPARAM) -> BOOL {
    let b = &mut *(l.0 as *mut Busca);
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if !b.pids.contains(&pid) || IsWindowVisible(h).as_bool() || GetWindow(h, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false) {
        return TRUE;
    }
    let mut c = [0u16; 80];
    let nc = GetClassNameW(h, &mut c);
    let classe = String::from_utf16_lossy(&c[..nc.max(0) as usize]);
    let cl = classe.to_lowercase();
    if ["ime", "notify", "power", "message", "tooltip", "crashpad", "systempreferences"].iter().any(|x| cl.contains(x)) {
        return TRUE;
    }
    if GetWindowTextLengthW(h) == 0 && classe != "Chrome_WidgetWin_1" {
        return TRUE;
    }
    let mut r = RECT::default();
    if GetWindowRect(h, &mut r).is_ok() {
        let (w, a) = (r.right - r.left, r.bottom - r.top);
        // minimizada fica com um tamanho minusculo: aceita-se, mas so se nao houver melhor
        if IsIconic(h).as_bool() && b.melhor == 0 {
            b.melhor = h.0 as isize;
        } else if w >= 300 && a >= 200 && (w as i64) * (a as i64) > b.area {
            b.area = (w as i64) * (a as i64);
            b.melhor = h.0 as isize;
        }
    }
    TRUE
}

/// App a correr escondida na bandeja (ex.: LM Studio, VoiceStudio): a janela principal existe
/// mas esta invisivel. Mostra-a e poe-na a frente; devolve-a (0 se nao houver nenhuma).
fn mostrar_escondida(processo: &str) -> isize {
    let pids = pids_de(processo);
    if pids.is_empty() {
        return 0;
    }
    let mut b = Busca { pids, melhor: 0, area: -1 };
    unsafe {
        let _ = EnumWindows(Some(cada_escondida), LPARAM(&mut b as *mut Busca as isize));
        if b.melhor != 0 {
            let h = hwnd(b.melhor);
            let _ = ShowWindow(h, SW_SHOW);
            let _ = ShowWindow(h, SW_RESTORE);
            frente(h);
        }
    }
    b.melhor
}

// ---------------------------------------------------------------- fechar a app quando se sai da pagina
// Fechar o separador fecha a app no PC. Espera FECHO_APOS sem
// nenhuma pagina dessa app aberta (recarregar, rede a falhar ou telemovel bloqueado nao fecham nada)
// e fecha com WM_CLOSE, como o X da janela: se a app perguntar "guardar?", a pergunta fica la.

const FECHO_APOS: Duration = Duration::from_secs(45);

/// processo -> (paginas abertas, geracao)
static SESSOES: std::sync::LazyLock<Mutex<HashMap<String, (u32, u64)>>> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn entrou(processo: &str) {
    let mut m = SESSOES.lock().unwrap_or_else(|e| e.into_inner());
    let e = m.entry(processo.to_lowercase()).or_insert((0, 0));
    e.0 += 1;
    e.1 += 1;
}

/// Devolve a geracao se esta era a ultima pagina aberta da app.
fn saiu(processo: &str) -> Option<u64> {
    let mut m = SESSOES.lock().unwrap_or_else(|e| e.into_inner());
    let e = m.entry(processo.to_lowercase()).or_insert((1, 0));
    e.0 = e.0.saturating_sub(1);
    (e.0 == 0).then_some(e.1)
}

/// Geracao atual se nao houver nenhuma pagina aberta desta app.
fn sozinha_agora(processo: &str) -> Option<u64> {
    let m = SESSOES.lock().unwrap_or_else(|e| e.into_inner());
    m.get(&processo.to_lowercase()).and_then(|e| (e.0 == 0).then_some(e.1))
}

/// A pagina avisa que o separador fechou (sendBeacon no pagehide). Fecha-se a app logo, sem os
/// 45 s de espera: so se aguardam 3 s para um simples recarregar da pagina nao a fechar.
async fn sair(State(e): State<Arc<Estado>>, h: HeaderMap, Query(q): Query<Q>) -> Response {
    if !autorizado(&e, &h) || q.t.as_deref() != Some(e.chave.as_str()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let id = q.app.unwrap_or_default();
    let Some(a) = e.cfg.app(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !a.fechar || a.processo.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
    nota(&format!("{}: separador fechado", a.nome));
    tokio::spawn(async move {
        // esperar que a ligacao desta pagina caia (ate 3 s) e depois 3 s sem nenhuma pagina
        let mut g = None;
        for _ in 0..30 {
            g = sozinha_agora(&a.processo);
            if g.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let Some(g) = g else { return }; // ainda ha outra pagina aberta desta app
        tokio::time::sleep(Duration::from_secs(3)).await;
        if ainda_sozinha(&a.processo, g) {
            let (pr, nome) = (a.processo.clone(), a.nome.clone());
            let _ = tokio::task::spawn_blocking(move || fechar_app(&pr, &nome)).await;
        }
    });
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------- ficheiros
// Enviar: o aparelho manda um ficheiro (arrastado para a pagina ou escolhido) e fica nas
// Transferencias do PC. Trazer: a pagina lista as Transferencias e o Ambiente de trabalho do PC e
// descarrega o escolhido. A chave vai no cabecalho x-chave (nunca no endereco); cada descarga usa
// uma senha de uso unico que dura 2 minutos.

/// Pasta conhecida do utilizador (o valor de "User Shell Folders", com %VARIAVEIS% expandidas).
fn pasta_utilizador(valor: &str, reserva: &str) -> std::path::PathBuf {
    use windows::Win32::System::Registry::*;
    let chave: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders\0".encode_utf16().collect();
    let nome: Vec<u16> = format!("{valor}\0").encode_utf16().collect();
    let mut buf = vec![0u16; 1024];
    let mut tam = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(chave.as_ptr()),
            windows::core::PCWSTR(nome.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, // REG_EXPAND_SZ sai ja expandido
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut tam),
        )
    };
    if r.is_ok() {
        let n = (tam as usize / 2).saturating_sub(1);
        let p = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        if !p.trim().is_empty() && std::path::Path::new(&p).is_dir() {
            return p.into();
        }
    }
    std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(reserva)
}

fn transferencias() -> std::path::PathBuf {
    pasta_utilizador("{374DE290-123F-4565-9164-39C4925E467B}", "Downloads")
}

fn ambiente() -> std::path::PathBuf {
    pasta_utilizador("Desktop", "Desktop")
}

/// "t" = Transferencias, "a" = Ambiente de trabalho
fn pasta_de(id: &str) -> Option<std::path::PathBuf> {
    match id {
        "t" => Some(transferencias()),
        "a" => Some(ambiente()),
        _ => None,
    }
}

/// So o nome do ficheiro, sem pastas nem caracteres que o Windows nao aceita.
fn nome_seguro(n: &str) -> String {
    let base = n.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base.chars().filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c)).collect();
    s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.chars().count() > 180 {
        s = s.chars().take(180).collect();
    }
    let raiz = s.split('.').next().unwrap_or("").to_uppercase();
    let reservado = matches!(raiz.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((raiz.starts_with("COM") || raiz.starts_with("LPT")) && raiz.len() == 4 && raiz.as_bytes()[3].is_ascii_digit());
    if s.is_empty() || s == "." || s == ".." {
        s = "ficheiro".into();
    }
    if reservado {
        s = format!("_{s}");
    }
    s
}

/// Nome livre na pasta: "foto.jpg", senao "foto (1).jpg", "foto (2).jpg"...
fn nome_livre(pasta: &std::path::Path, nome: &str) -> std::path::PathBuf {
    let p = pasta.join(nome);
    if !p.exists() {
        return p;
    }
    let (base, ext) = match nome.rfind('.') {
        Some(i) if i > 0 => (&nome[..i], &nome[i..]),
        _ => (nome, ""),
    };
    for i in 1..10000 {
        let p = pasta.join(format!("{base} ({i}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    pasta.join(format!("{base} ({}){ext}", std::process::id()))
}

fn chave_ok(e: &Estado, h: &HeaderMap) -> bool {
    autorizado(e, h) && h.get("x-chave").and_then(|v| v.to_str().ok()) == Some(e.chave.as_str())
}

#[derive(Deserialize)]
struct QNome {
    nome: Option<String>,
}

/// POST /enviar?nome=foto.jpg (corpo = o ficheiro) -> Transferencias do PC
async fn receber_ficheiro(State(e): State<Arc<Estado>>, h: HeaderMap, Query(q): Query<QNome>, corpo: axum::body::Body) -> Response {
    use tokio::io::AsyncWriteExt;
    if !chave_ok(&e, &h) {
        nota(&format!("403 enviar {}", ip_de(&h)));
        return StatusCode::FORBIDDEN.into_response();
    }
    let nome = nome_seguro(&q.nome.unwrap_or_default());
    let pasta = transferencias();
    let destino = nome_livre(&pasta, &nome);
    // escreve-se num ficheiro .parcial e so no fim fica com o nome certo
    let parcial = destino.with_file_name(format!("{}.kcu-parcial", destino.file_name().unwrap_or_default().to_string_lossy()));
    let mut f = match tokio::fs::File::create(&parcial).await {
        Ok(f) => f,
        Err(err) => {
            nota(&format!("enviar: nao consegui criar {}: {err}", parcial.display()));
            return (StatusCode::INTERNAL_SERVER_ERROR, "Nao consegui gravar no PC.").into_response();
        }
    };
    let mut s = corpo.into_data_stream();
    let mut total: u64 = 0;
    let mut falhou = None;
    while let Some(b) = s.next().await {
        match b {
            Ok(b) => {
                total += b.len() as u64;
                if let Err(err) = f.write_all(&b).await {
                    falhou = Some(err.to_string());
                    break;
                }
            }
            Err(err) => {
                falhou = Some(err.to_string());
                break;
            }
        }
    }
    let _ = f.flush().await;
    drop(f);
    if let Some(err) = falhou {
        let _ = tokio::fs::remove_file(&parcial).await;
        nota(&format!("enviar {nome}: interrompido ({err})"));
        return (StatusCode::BAD_REQUEST, "Envio interrompido.").into_response();
    }
    // outro envio com o mesmo nome pode ter acabado entretanto
    let destino = if destino.exists() { nome_livre(&pasta, &nome) } else { destino };
    if let Err(err) = tokio::fs::rename(&parcial, &destino).await {
        let _ = tokio::fs::remove_file(&parcial).await;
        nota(&format!("enviar {nome}: nao consegui renomear: {err}"));
        return (StatusCode::INTERNAL_SERVER_ERROR, "Nao consegui gravar no PC.").into_response();
    }
    let final_ = destino.file_name().unwrap_or_default().to_string_lossy().to_string();
    nota(&format!("{} enviou {final_} ({total} bytes) para as Transferencias", ip_de(&h)));
    axum::Json(serde_json::json!({ "nome": final_, "bytes": total })).into_response()
}

/// GET /lista -> ficheiros das Transferencias e do Ambiente de trabalho (os mais recentes primeiro)
async fn lista(State(e): State<Arc<Estado>>, h: HeaderMap) -> Response {
    if !chave_ok(&e, &h) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let v = tokio::task::spawn_blocking(|| {
        let mut v: Vec<(String, String, u64, u64)> = Vec::new();
        for (id, p) in [("t", transferencias()), ("a", ambiente())] {
            let Ok(rd) = std::fs::read_dir(&p) else { continue };
            for en in rd.flatten() {
                let Ok(md) = en.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let nome = en.file_name().to_string_lossy().to_string();
                let l = nome.to_lowercase();
                if l == "desktop.ini" || l.ends_with(".kcu-parcial") || l.ends_with(".crdownload") || l.ends_with(".part") || l.ends_with(".tmp") {
                    continue;
                }
                let quando = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                v.push((id.to_string(), nome, md.len(), quando));
            }
        }
        v.sort_by(|a, b| b.3.cmp(&a.3));
        v.truncate(80);
        v
    })
    .await
    .unwrap_or_default();
    let j: Vec<_> = v.into_iter().map(|(p, n, t, q)| serde_json::json!({ "pasta": p, "nome": n, "bytes": t, "quando": q })).collect();
    let mut r = axum::Json(j).into_response();
    r.headers_mut().insert("cache-control", "no-store".parse().unwrap());
    r
}

/// senha de uso unico -> (caminho, valida ate)
static SENHAS: std::sync::LazyLock<Mutex<HashMap<String, (std::path::PathBuf, Instant)>>> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn senha_nova() -> String {
    // 128 bits do gerador do sistema
    let mut b = [0u8; 16];
    unsafe {
        let _ = windows::Win32::Security::Cryptography::BCryptGenRandom(
            None,
            &mut b,
            windows::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        );
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Deserialize)]
struct Pedido {
    pasta: String,
    nome: String,
}

/// POST /pedir {"pasta":"t","nome":"x.pdf"} -> {"k": senha} para GET /descarregar?k=senha
async fn pedir(State(e): State<Arc<Estado>>, h: HeaderMap, axum::Json(p): axum::Json<Pedido>) -> Response {
    if !chave_ok(&e, &h) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(pasta) = pasta_de(&p.pasta) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // so um nome simples dessa pasta (nada de ..\ nem caminhos)
    if p.nome.is_empty() || p.nome != nome_seguro(&p.nome) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let caminho = pasta.join(&p.nome);
    if !caminho.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let k = senha_nova();
    {
        let mut m = SENHAS.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, (_, ate)| *ate > Instant::now());
        m.insert(k.clone(), (caminho, Instant::now() + Duration::from_secs(120)));
    }
    axum::Json(serde_json::json!({ "k": k })).into_response()
}

#[derive(Deserialize)]
struct QSenha {
    k: Option<String>,
}

/// GET /descarregar?k=senha -> o ficheiro (a senha so serve uma vez)
async fn descarregar(State(e): State<Arc<Estado>>, h: HeaderMap, Query(q): Query<QSenha>) -> Response {
    if !autorizado(&e, &h) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let k = q.k.unwrap_or_default();
    let item = {
        let mut m = SENHAS.lock().unwrap_or_else(|e| e.into_inner());
        m.remove(&k).filter(|(_, ate)| *ate > Instant::now())
    };
    let Some((caminho, _)) = item else {
        return (StatusCode::GONE, "Este pedido ja foi usado ou expirou. Volta a carregar no ficheiro.").into_response();
    };
    let f = match tokio::fs::File::open(&caminho).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let tam = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let nome = caminho.file_name().unwrap_or_default().to_string_lossy().to_string();
    nota(&format!("{} descarregou {nome} ({tam} bytes)", ip_de(&h)));
    // nome em UTF-8 (RFC 5987) e uma versao so ASCII para browsers antigos
    let ascii: String = nome.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let utf8: String = nome
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    let corpo = axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(f, 256 * 1024));
    let mut r = corpo.into_response();
    let hs = r.headers_mut();
    hs.insert("content-type", "application/octet-stream".parse().unwrap());
    if let Ok(v) = tam.to_string().parse() {
        hs.insert("content-length", v);
    }
    if let Ok(v) = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{utf8}").parse() {
        hs.insert("content-disposition", v);
    }
    hs.insert("cache-control", "no-store".parse().unwrap());
    r
}

fn ainda_sozinha(processo: &str, geracao: u64) -> bool {
    let m = SESSOES.lock().unwrap_or_else(|e| e.into_inner());
    m.get(&processo.to_lowercase()).map(|e| e.0 == 0 && e.1 == geracao).unwrap_or(false)
}

unsafe extern "system" fn junta(h: HWND, l: LPARAM) -> BOOL {
    let b = &mut *(l.0 as *mut (Vec<u32>, Vec<isize>));
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if b.0.contains(&pid) && IsWindowVisible(h).as_bool() && !GetWindow(h, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false) {
        b.1.push(h.0 as isize);
    }
    TRUE
}

/// Se a app continuar a correr mas SEM nenhuma janela visivel (escondida na bandeja), termina-a.
/// Se ainda tiver uma janela visivel (ex.: "guardar alteracoes?"), nao se mexe em nada.
fn terminar_escondida(processo: &str, nome: &str) {
    let pids = pids_de(processo);
    if pids.is_empty() || !janelas_de_pids(pids.clone()).is_empty() {
        return;
    }
    use std::os::windows::process::CommandExt;
    let mut args: Vec<String> = vec!["/F".into(), "/T".into()];
    for p in &pids {
        args.push("/PID".into());
        args.push(p.to_string());
    }
    let _ = std::process::Command::new("taskkill")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .status();
    nota(&format!("{nome} ficou escondida a correr sem janela: terminei"));
}

/// Fecha a app como o X: WM_CLOSE a cada janela principal visivel dela; passados 10 s,
/// se so restar o processo escondido (bandeja), termina-o para libertar a RAM.
fn fechar_app(processo: &str, nome: &str) {
    let mut b = (pids_de(processo), Vec::<isize>::new());
    if b.0.is_empty() {
        return;
    }
    unsafe {
        let _ = EnumWindows(Some(junta), LPARAM(&mut b as *mut _ as isize));
        for h in &b.1 {
            let _ = PostMessageW(hwnd(*h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    nota(&format!("fechei {nome} ({} janelas) por ninguem a estar a ver", b.1.len()));
    std::thread::sleep(Duration::from_secs(10));
    terminar_escondida(processo, nome);
}

fn janela_de(processo: &str) -> isize {
    let pids = pids_de(processo);
    if pids.is_empty() {
        return 0;
    }
    // se uma janela da app estiver a frente (ex.: uma janela secundaria que ela abriu), e essa
    unsafe {
        let fg = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if !fg.0.is_null()
            && pids.contains(&pid)
            && IsWindowVisible(fg).as_bool()
            && !GetWindow(fg, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false)
            && GetWindowTextLengthW(fg) > 0
        {
            return fg.0 as isize;
        }
    }
    let mut b = Busca { pids, melhor: 0, area: -1 };
    unsafe {
        let _ = EnumWindows(Some(cada), LPARAM(&mut b as *mut Busca as isize));
    }
    b.melhor
}

/// Limites visiveis da janela (sem a sombra invisivel do Windows 10/11).
fn limites(h: HWND) -> Option<RECT> {
    unsafe {
        let mut r = RECT::default();
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut c_void, std::mem::size_of::<RECT>() as u32).is_ok()
            && r.right > r.left
        {
            return Some(r);
        }
        let mut w = RECT::default();
        GetWindowRect(h, &mut w).ok()?;
        Some(w)
    }
}

/// Captura a janela inteira (mesmo tapada por outras) e corta a sombra. BGRA, linhas de cima para baixo.
fn capturar(h: HWND) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut w = RECT::default();
        GetWindowRect(h, &mut w).ok()?;
        let f = limites(h)?;
        let (ww, wh) = (w.right - w.left, w.bottom - w.top);
        if ww <= 0 || wh <= 0 {
            return None;
        }
        let ecra = GetDC(HWND::default());
        let dc = CreateCompatibleDC(ecra);
        let bmp = CreateCompatibleBitmap(ecra, ww, wh);
        let velho = SelectObject(dc, bmp);
        let ok = PrintWindow(h, dc, PRINT_WINDOW_FLAGS(2)).as_bool(); // PW_RENDERFULLCONTENT
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ww,
                biHeight: -wh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (ww * wh * 4) as usize];
        let linhas = GetDIBits(dc, bmp, 0, wh as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
        SelectObject(dc, velho);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(dc);
        ReleaseDC(HWND::default(), ecra);
        if !ok || linhas == 0 {
            return None;
        }
        // cortar a sombra
        let (ox, oy) = ((f.left - w.left).max(0), (f.top - w.top).max(0));
        let (cw, ch) = ((f.right - f.left).min(ww - ox), (f.bottom - f.top).min(wh - oy));
        if cw <= 0 || ch <= 0 {
            return None;
        }
        let mut out = Vec::with_capacity((cw * ch * 4) as usize);
        for y in oy..oy + ch {
            let i = ((y * ww + ox) * 4) as usize;
            out.extend_from_slice(&buf[i..i + (cw * 4) as usize]);
        }
        Some((cw as u32, ch as u32, out))
    }
}

/// Reduz para caber em `max` de largura (para telemoveis).
fn reduzir(w: u32, h: u32, px: Vec<u8>, max: u32) -> (u32, u32, Vec<u8>) {
    if max == 0 || w <= max {
        return (w, h, px);
    }
    let nh = ((h as u64 * max as u64) / w as u64).max(1) as u32;
    let mut out = vec![0u8; (max * nh * 4) as usize];
    for y in 0..nh {
        let sy = (y as u64 * h as u64 / nh as u64) as u32;
        for x in 0..max {
            let sx = (x as u64 * w as u64 / max as u64) as u32;
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * max + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&px[s..s + 4]);
        }
    }
    (max, nh, out)
}

// ---------------------------------------------------------------- captura moderna (Windows Graphics Capture)
// Apanha qualquer janela (WebView2, GPU, tapada por outras). O PrintWindow fica como reserva.

type Quadro = Arc<Mutex<Option<(u32, u32, Vec<u8>, u64)>>>;

struct Wgc {
    quadro: Quadro,
    n: u64,
    tmp: Vec<u8>,
}

impl windows_capture::capture::GraphicsCaptureApiHandler for Wgc {
    type Flags = Quadro;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { quadro: ctx.flags, n: 0, tmp: Vec::new() })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        _c: windows_capture::graphics_capture_api::InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), Self::Error> {
            let b = frame.buffer()?;
            let (w, h) = (b.width(), b.height());
            let px = b.as_nopadding_buffer(&mut self.tmp).to_vec();
            if px.len() != (w * h * 4) as usize {
                return Ok(()); // quadro estranho (a mudar de tamanho): ignorar
            }
            self.n += 1;
            if let Ok(mut g) = self.quadro.lock() {
                *g = Some((w, h, px, self.n));
            }
            Ok(())
        }));
        match r {
            Ok(x) => x,
            Err(_) => Ok(()),
        }
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

type ControloWgc = windows_capture::capture::CaptureControl<Wgc, Box<dyn std::error::Error + Send + Sync>>;

fn iniciar_wgc(h: isize, quadro: Quadro) -> Option<ControloWgc> {
    use windows_capture::capture::GraphicsCaptureApiHandler;
    use windows_capture::settings::*;
    let janela = windows_capture::window::Window::from_raw_hwnd(h as *mut c_void);
    let s = Settings::new(
        janela,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Include,
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16)),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        quadro,
    );
    match Wgc::start_free_threaded(s) {
        Ok(c) => Some(c),
        Err(e) => {
            nota(&format!("WGC falhou ({e}); a usar PrintWindow"));
            None
        }
    }
}

// ---------------------------------------------------------------- ecra inteiro (monitor principal)

const ICONE_ECRA: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="4" width="19" height="13" rx="2"/><path d="M8 21h8M12 17v4"/></svg>"##;

/// Retangulo do monitor principal, em pixeis reais (o processo e "per monitor DPI aware").
fn ret_ecra() -> RECT {
    unsafe {
        let m = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(m, &mut mi).as_bool() {
            return mi.rcMonitor;
        }
        RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }
    }
}

fn iniciar_wgc_ecra(quadro: Quadro) -> Option<ControloWgc> {
    use windows_capture::capture::GraphicsCaptureApiHandler;
    use windows_capture::settings::*;
    let m = match windows_capture::monitor::Monitor::primary() {
        Ok(m) => m,
        Err(e) => {
            nota(&format!("WGC: sem monitor principal ({e}); a usar BitBlt"));
            return None;
        }
    };
    let s = Settings::new(
        m,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16)),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        quadro,
    );
    match Wgc::start_free_threaded(s) {
        Ok(c) => Some(c),
        Err(e) => {
            nota(&format!("WGC do ecra falhou ({e}); a usar BitBlt"));
            None
        }
    }
}

/// Reserva: copia o monitor principal com BitBlt. BGRA, linhas de cima para baixo.
fn capturar_ecra() -> Option<(u32, u32, Vec<u8>)> {
    let r = ret_ecra();
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let ecra = GetDC(HWND::default());
        let dc = CreateCompatibleDC(ecra);
        let bmp = CreateCompatibleBitmap(ecra, w, h);
        let velho = SelectObject(dc, bmp);
        let ok = BitBlt(dc, 0, 0, w, h, ecra, r.left, r.top, SRCCOPY | CAPTUREBLT).is_ok();
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let linhas = GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
        SelectObject(dc, velho);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(dc);
        ReleaseDC(HWND::default(), ecra);
        if !ok || linhas == 0 {
            return None;
        }
        Some((w as u32, h as u32, buf))
    }
}

fn assinatura(px: &[u8]) -> u64 {
    let mut s = DefaultHasher::new();
    px.iter().step_by(97).for_each(|b| b.hash(&mut s));
    px.len().hash(&mut s);
    s.finish()
}

// ---------------------------------------------------------------- entrada

fn enviar(i: &[INPUT]) {
    unsafe {
        SendInput(i, std::mem::size_of::<INPUT>() as i32);
    }
}

fn tecla(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn rato(flags: MOUSE_EVENT_FLAGS, dados: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: dados as _, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// Traz a janela para a frente (o Windows so deixa depois de uma tecla "fantasma").
/// Se a janela da frente ja for da propria app (um dialogo, um menu, outra janela dela), nao se
/// mexe: antes cada tecla mandava um Alt e roubava o foco ao dialogo.
fn frente(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let fg = GetForegroundWindow();
        if fg != h && !fg.0.is_null() && pid_da_janela(fg) == pid_da_janela(h) {
            return;
        }
        if fg != h {
            // juntar a entrada deste fio a da janela da frente: o Windows deixa entao mudar a
            // janela da frente sem carregar em tecla nenhuma (o Alt "fantasma" de antes abria o
            // menu das apps e as letras seguintes escolhiam opcoes do menu)
            use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
            let eu = GetCurrentThreadId();
            let dela = if fg.0.is_null() { 0 } else { GetWindowThreadProcessId(fg, None) };
            let junto = dela != 0 && dela != eu && AttachThreadInput(eu, dela, true).as_bool();
            let _ = BringWindowToTop(h);
            let _ = SetForegroundWindow(h);
            if junto {
                let _ = AttachThreadInput(eu, dela, false);
            }
            if GetForegroundWindow() != h {
                // ultimo recurso: Alt carregado SO enquanto se muda a janela, e uma tecla neutra
                // depois para o Alt largado nao abrir o menu da app
                enviar(&[tecla(VK_MENU.0, 0, KEYBD_EVENT_FLAGS(0))]);
                let _ = SetForegroundWindow(h);
                enviar(&[tecla(0xE8, 0, KEYBD_EVENT_FLAGS(0)), tecla(0xE8, 0, KEYEVENTF_KEYUP), tecla(VK_MENU.0, 0, KEYEVENTF_KEYUP)]);
            }
        }
    }
}

/// Se no ponto (px, py) estiver uma janela de OUTRO programa por cima da app, poe a app por cima
/// de tudo (o clique iria para essa outra janela, p.ex. um "Executar" esquecido no ecra).
fn destapar(h: HWND, px: i32, py: i32) {
    unsafe {
        let w = WindowFromPoint(POINT { x: px, y: py });
        if w.0.is_null() {
            return;
        }
        let raiz = GetAncestor(w, GA_ROOT);
        if raiz == h || pid_da_janela(raiz) == pid_da_janela(h) {
            return;
        }
        let f = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
        let _ = SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, f);
        let _ = SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, f);
        frente(h);
    }
}

fn estendida(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0xA3 | 0xA5 | 0x6F | 0x90)
}

#[derive(Deserialize)]
struct Ev {
    t: String,
    #[serde(default)]
    a: String,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    #[serde(default)]
    b: u8,
    #[serde(default)]
    d: i32,
    #[serde(default)]
    vk: u16,
    #[serde(default)]
    s: String,
    #[serde(default)]
    w: i32,
    #[serde(default)]
    h: i32,
}

/// `ecra`: modo ecra inteiro (sem janela alvo: o rato vai para o monitor e o teclado para quem
/// estiver a frente no PC).
fn aplicar(h: HWND, ecra: bool, ev: &Ev) {
    if h.0.is_null() && !ecra {
        return;
    }
    let frente = |h: HWND| {
        if !ecra {
            frente(h);
        }
    };
    match ev.t.as_str() {
        "m" => {
            let f = if ecra {
                ret_ecra()
            } else {
                let Some(f) = limites(h) else { return };
                f
            };
            let px = f.left + ((f.right - f.left) as f64 * ev.x.clamp(0.0, 0.9999)) as i32;
            let py = f.top + ((f.bottom - f.top) as f64 * ev.y.clamp(0.0, 0.9999)) as i32;
            if ev.a == "down" && !ecra {
                frente(h);
                destapar(h, px, py);
            }
            unsafe {
                let _ = SetCursorPos(px, py);
            }
            let (baixo, cima) = match ev.b {
                2 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                1 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            };
            match ev.a.as_str() {
                "down" => enviar(&[rato(baixo, 0)]),
                "up" => enviar(&[rato(cima, 0)]),
                "wheel" => enviar(&[rato(MOUSEEVENTF_WHEEL, ev.d)]),
                "hwheel" => enviar(&[rato(MOUSEEVENTF_HWHEEL, ev.d)]),
                _ => {}
            }
        }
        "k" => {
            frente(h);
            let scan = unsafe { MapVirtualKeyW(ev.vk as u32, MAPVK_VK_TO_VSC) } as u16;
            let mut fl = if ev.a == "up" { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
            if estendida(ev.vk) {
                fl |= KEYEVENTF_EXTENDEDKEY;
            }
            enviar(&[tecla(ev.vk, scan, fl)]);
        }
        "c" => {
            frente(h);
            let mut v = Vec::new();
            for ch in ev.s.chars() {
                if ch == '\n' {
                    // mudanca de linha como Enter a serio (em Unicode muitas apps ignoram-na)
                    v.push(tecla(0x0D, 0x1C, KEYBD_EVENT_FLAGS(0)));
                    v.push(tecla(0x0D, 0x1C, KEYEVENTF_KEYUP));
                    continue;
                }
                if ch == '\r' {
                    continue;
                }
                let mut b = [0u16; 2];
                for u in ch.encode_utf16(&mut b).iter() {
                    v.push(tecla(0, *u, KEYEVENTF_UNICODE));
                    v.push(tecla(0, *u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
            }
            enviar(&v);
        }
        "v" => {
            // colar o texto do aparelho: poe-no na area de transferencia do PC e carrega Ctrl+V
            frente(h);
            if por_na_area(&ev.s) {
                std::thread::sleep(Duration::from_millis(40));
                let (c, v) = (VK_CONTROL.0, 0x56u16);
                let sc = |vk: u16| unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
                enviar(&[
                    tecla(c, sc(c), KEYBD_EVENT_FLAGS(0)),
                    tecla(v, sc(v), KEYBD_EVENT_FLAGS(0)),
                    tecla(v, sc(v), KEYEVENTF_KEYUP),
                    tecla(c, sc(c), KEYEVENTF_KEYUP),
                ]);
            }
        }
        _ if ecra => {} // ajustar/maximizar nao se aplicam ao ecra inteiro
        "r" => unsafe {
            // ajustar o tamanho da janela ao do aparelho (sem mexer na posicao)
            if ev.w >= 320 && ev.h >= 240 {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetWindowPos(h, HWND::default(), 0, 0, ev.w.min(3840), ev.h.min(2160), SWP_NOZORDER | SWP_NOMOVE);
            }
        },
        "max" => unsafe {
            let _ = ShowWindow(h, SW_MAXIMIZE);
        },
        _ => {}
    }
}

/// Abre a app como um duplo clique (`start` pelo cmd, sem herdar nada deste processo).
/// Lancado diretamente, o Obsidian ficava preso sem janela. Se a app ja estiver a correr
/// escondida (sem janela), isto volta a mostrar a janela dela.
///
/// Varias apps abertas ao mesmo tempo: uma app Chromium (Electron ou WebView2) tapada por
/// outra janela deixa de se desenhar e a captura fica cinzenta. Por isso as apps Electron
/// arrancam com SEM_PAUSA e as WebView2 (ex.: apps Tauri) recebem-no pela variavel
/// WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS. So vale para apps lancadas daqui: uma que ja
/// estava aberta tem de ser fechada e aberta outra vez.
const SEM_PAUSA: &str = "--disable-features=CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows";

fn e_electron(exe: &str) -> bool {
    let d = std::path::Path::new(exe).parent().map(|p| p.to_path_buf()).unwrap_or_default();
    d.join("chrome_100_percent.pak").exists() || d.join("resources").join("app.asar").exists()
}

fn lancar(a: &App) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let dir = std::path::Path::new(&a.exe).parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let extra = if e_electron(&a.exe) { format!(" {SEM_PAUSA}") } else { String::new() };
    std::process::Command::new("cmd")
        .raw_arg(format!("/c start \"\" \"{}\"{extra}", a.exe))
        .env("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", SEM_PAUSA)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .map(|_| ())
}

unsafe extern "system" fn junta_executar(h: HWND, l: LPARAM) -> BOOL {
    let v = &mut *(l.0 as *mut Vec<isize>);
    let mut c = [0u16; 64];
    let nc = GetClassNameW(h, &mut c);
    let mut t = [0u16; 64];
    let nt = GetWindowTextW(h, &mut t);
    let classe = String::from_utf16_lossy(&c[..nc.max(0) as usize]);
    let titulo = String::from_utf16_lossy(&t[..nt.max(0) as usize]);
    if classe == "#32770" && (titulo == "Executar" || titulo == "Run") && IsWindowVisible(h).as_bool() {
        v.push(h.0 as isize);
    }
    TRUE
}

/// Fecha caixas "Executar" (Win+R) abertas no ecra; tapavam as apps e roubavam os cliques.
fn fechar_executar() {
    let mut v: Vec<isize> = Vec::new();
    let explorer = pids_de("explorer.exe");
    unsafe {
        let _ = EnumWindows(Some(junta_executar), LPARAM(&mut v as *mut _ as isize));
    }
    let mut n = 0;
    for h in v {
        if explorer.contains(&pid_da_janela(hwnd(h))) {
            fechar_janela(h);
            n += 1;
        }
    }
    if n > 0 {
        nota(&format!("fechei {n} janela(s) Executar esquecida(s)"));
    }
}

// ---------------------------------------------------------------- video H.264 (NVENC)
// O ffmpeg recebe as imagens BGRA pela entrada e devolve H.264 Annex B com um AUD antes de cada
// imagem; cada imagem (access unit) vai inteira numa mensagem do WebSocket e o browser descodifica
// com WebCodecs. Muito menos rede que JPEG, e da 1080p a 60.

struct Codificador {
    filho: std::process::Child,
    // as imagens vao ao ffmpeg por um fio proprio: se o ffmpeg (NVENC) encravar, a captura nao
    // fica presa no write e da para passar para JPEG
    entrada: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    w: u32,
    h: u32,
    criado: Instant,
    // ms desde `criado` da ultima imagem H.264 que saiu do ffmpeg (0 = ainda nenhuma)
    saiu: Arc<std::sync::atomic::AtomicU64>,
}

impl Drop for Codificador {
    fn drop(&mut self) {
        drop(self.entrada.take());
        let _ = self.filho.kill();
        let _ = self.filho.wait();
    }
}

impl Codificador {
    fn novo(ffmpeg: &str, w: u32, h: u32, kbps: u32, tx: tokio::sync::mpsc::Sender<Saida>) -> Option<Self> {
        use std::os::windows::process::CommandExt;
        let tamanho = format!("{w}x{h}");
        let taxa = format!("{kbps}k");
        let pico = format!("{}k", kbps * 2);
        let mut c = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error",
                "-fflags", "nobuffer", "-flags", "low_delay",
                "-f", "rawvideo", "-pix_fmt", "bgra", "-s", &tamanho, "-framerate", "60", "-i", "-",
                "-c:v", "h264_nvenc", "-preset", "p2", "-tune", "ull", "-zerolatency", "1", "-delay", "0",
                "-rc", "cbr", "-b:v", &taxa, "-maxrate", &pico, "-bufsize", &taxa,
                "-g", "120", "-bf", "0", "-profile:v", "high", "-pix_fmt", "yuv420p",
                "-bsf:v", "h264_metadata=aud=insert",
                "-flush_packets", "1", "-f", "h264", "-",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(r"D:\kcu\kcu-janelas-ffmpeg.log")
                    .map(std::process::Stdio::from)
                    .unwrap_or_else(|_| std::process::Stdio::null()),
            )
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|e| nota(&format!("ffmpeg nao arrancou: {e}")))
            .ok()?;
        let saida = c.stdout.take()?;
        let mut stdin = c.stdin.take()?;
        let criado = Instant::now();
        let saiu = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let s2 = saiu.clone();
        std::thread::spawn(move || ler_h264(saida, tx, criado, s2));
        let (etx, erx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        std::thread::spawn(move || {
            use std::io::Write as _;
            while let Ok(px) = erx.recv() {
                if stdin.write_all(&px).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        Some(Self { filho: c, entrada: Some(etx), w, h, criado, saiu })
    }

    /// false = o ffmpeg morreu. Com o ffmpeg ocupado a imagem e deitada fora (vem outra a seguir).
    fn imagem(&mut self, px: Vec<u8>) -> bool {
        match self.entrada.as_ref() {
            Some(e) => !matches!(e.try_send(px), Err(std::sync::mpsc::TrySendError::Disconnected(_))),
            None => false,
        }
    }

    /// Sem H.264 a sair ha mais de 5 s (desde o arranque ou desde a ultima imagem).
    fn encravado(&self) -> bool {
        let ultima = self.saiu.load(std::sync::atomic::Ordering::Relaxed);
        self.criado.elapsed().as_millis() as u64 > ultima + 5000
    }
}

/// Le o H.264 do ffmpeg e manda cada access unit (de AUD a AUD) inteiro.
fn ler_h264(mut r: std::process::ChildStdout, tx: tokio::sync::mpsc::Sender<Saida>, criado: Instant, saiu: Arc<std::sync::atomic::AtomicU64>) {
    use std::io::Read as _;
    const AUD: [u8; 5] = [0, 0, 0, 1, 9];
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 1 << 16];
    loop {
        let n = match r.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        loop {
            let mut it = buf.windows(5).enumerate().filter(|(_, w)| *w == AUD).map(|(i, _)| i);
            let (Some(a), Some(b)) = (it.next(), it.next()) else { break };
            let au = buf[a..b].to_vec();
            buf.drain(..b);
            saiu.store(criado.elapsed().as_millis().max(1) as u64, std::sync::atomic::Ordering::Relaxed);
            if tx.blocking_send(Saida::H264(au)).is_err() {
                return;
            }
        }
    }
}

/// O H.264 (yuv420) quer largura e altura pares: corta 1 pixel se for preciso.
fn pares(w: u32, h: u32, px: Vec<u8>) -> (u32, u32, Vec<u8>) {
    let (ew, eh) = (w & !1, h & !1);
    if ew == w && eh == h {
        return (w, h, px);
    }
    let mut out = Vec::with_capacity((ew * eh * 4) as usize);
    for y in 0..eh {
        let i = (y * w * 4) as usize;
        out.extend_from_slice(&px[i..i + (ew * 4) as usize]);
    }
    (ew, eh, out)
}

// ---------------------------------------------------------------- som da app (so o dela)
// WASAPI "process loopback" (Windows 10 2004+): apanha so o som da arvore de processos da app
// (as apps Electron/Chromium tocam num processo filho, por isso conta a arvore toda), nunca o
// resto do PC. Vai em PCM 16 bits, 48 kHz, estereo, blocos de 20 ms, em mensagens binarias que
// comecam por 'A' (o video comeca por 00 00 00 01 ou FF D8). So se manda quando a pagina pede
// ({"t":"som","d":1}) e nunca silencio.

const SOM_BLOCO: usize = 960 * 4; // 20 ms a 48 kHz, 2 canais, 16 bits

/// Processo "raiz" da app: o que nao tem como pai outro processo da app.
fn raiz_de(processo: &str) -> Option<u32> {
    let pids = pids_de(processo);
    if pids.is_empty() {
        return None;
    }
    let mut pais: HashMap<u32, u32> = HashMap::new();
    unsafe {
        if let Ok(s) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut pe = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(s, &mut pe).is_ok() {
                loop {
                    pais.insert(pe.th32ProcessID, pe.th32ParentProcessID);
                    if Process32NextW(s, &mut pe).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(s);
        }
    }
    pids.iter().copied().find(|p| pais.get(p).map(|pai| !pids.contains(pai)).unwrap_or(true))
}

fn parado(p: &Mutex<bool>) -> bool {
    *p.lock().unwrap_or_else(|e| e.into_inner())
}

/// Fio do som de uma sessao: espera que a pagina o ligue e que a app exista, e capta-a.
/// Se a app for fechada e aberta outra vez (outro processo), volta a apanha-la.
fn som(processo: String, parar: Arc<Mutex<bool>>, ligado: Arc<AtomicBool>, tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
    let _ = wasapi::initialize_mta();
    let mut avisado = false;
    loop {
        if parado(&parar) || tx.is_closed() {
            return;
        }
        if !ligado.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        // ecra inteiro (processo vazio): todo o som do PC menos o deste programa
        let pid = if processo.is_empty() { Some(std::process::id()) } else { raiz_de(&processo) };
        let Some(pid) = pid else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        if let Err(e) = captar_som(&processo, pid, &parar, &ligado, &tx) {
            if !avisado {
                nota(&format!("som de {processo} (pid {pid}) falhou: {e}"));
                avisado = true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

fn captar_som(
    processo: &str,
    pid: u32,
    parar: &Mutex<bool>,
    ligado: &AtomicBool,
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use wasapi::*;
    let fmt = WaveFormat::new(16, 16, SampleType::Int, 48000, 2, None);
    let todo = processo.is_empty();
    // include_tree=false: tudo MENOS esta arvore de processos (o ecra inteiro exclui-se a si proprio)
    let mut c = AudioClient::new_application_loopback_client(pid, !todo)?;
    c.initialize_client(&fmt, Direction::Capture, StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 }, None)?;
    let ev = c.set_get_eventhandle()?;
    let cap = c.get_audiocaptureclient()?;
    let mut fila: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    c.start_stream()?;
    nota(&format!("som de {processo} ligado (pid {pid})"));
    let mut verif = Instant::now();
    'fora: loop {
        if parado(parar) || tx.is_closed() || !ligado.load(Ordering::Relaxed) {
            break;
        }
        // a app fechou ou abriu outra vez (outro processo): recomecar com o novo
        if !todo && verif.elapsed() > Duration::from_secs(2) {
            verif = Instant::now();
            if raiz_de(processo) != Some(pid) {
                break;
            }
        }
        while cap.get_next_packet_size()?.unwrap_or(0) > 0 {
            cap.read_from_device_to_deque(&mut fila)?;
        }
        while fila.len() >= SOM_BLOCO {
            let mut m = Vec::with_capacity(SOM_BLOCO + 1);
            m.push(b'A');
            m.extend(fila.drain(..SOM_BLOCO));
            // silencio (ou so ruido de +-8 em 32767): nao gastar rede
            if m[1..].chunks_exact(2).all(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() <= 8) {
                continue;
            }
            if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = tx.try_send(m) {
                break 'fora; // (cheia = rede lenta: deita-se este bloco fora)
            }
        }
        let _ = ev.wait_for_event(200); // sem som a app pode nao dar eventos
    }
    let _ = c.stop_stream();
    Ok(())
}

// ---------------------------------------------------------------- area de transferencia
// Colar: o texto do aparelho vai para a area de transferencia do PC e carrega-se Ctrl+V.
// Copiar: durante a sessao, quando o texto copiado no PC muda (Ctrl+C na app), vai para a pagina,
// que o poe na area de transferencia do aparelho. O que nos proprios pusemos nao volta para tras.

static AREA_NOSSA: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const CF_TEXTO: u32 = 13; // CF_UNICODETEXT

fn abrir_area() -> bool {
    use windows::Win32::System::DataExchange::OpenClipboard;
    for _ in 0..10 {
        if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20)); // outra app tem-na aberta
    }
    false
}

fn por_na_area(s: &str) -> bool {
    use windows::Win32::System::{DataExchange::*, Memory::*};
    let t: Vec<u16> = s.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
    if !abrir_area() {
        nota("colar: area de transferencia ocupada");
        return false;
    }
    let ok = unsafe {
        let _ = EmptyClipboard();
        match GlobalAlloc(GMEM_MOVEABLE, t.len() * 2) {
            Ok(g) => {
                let p = GlobalLock(g) as *mut u16;
                if p.is_null() {
                    false
                } else {
                    std::ptr::copy_nonoverlapping(t.as_ptr(), p, t.len());
                    let _ = GlobalUnlock(g);
                    SetClipboardData(CF_TEXTO, HANDLE(g.0)).is_ok()
                }
            }
            Err(_) => false,
        }
    };
    unsafe {
        let _ = CloseClipboard();
        AREA_NOSSA.store(GetClipboardSequenceNumber(), Ordering::Relaxed);
    }
    ok
}

fn ler_area() -> Option<String> {
    use windows::Win32::System::{DataExchange::*, Memory::*};
    if !abrir_area() {
        return None;
    }
    let r = unsafe {
        match GetClipboardData(CF_TEXTO) {
            Ok(h) if !h.0.is_null() => {
                let g = HGLOBAL(h.0);
                let p = GlobalLock(g) as *const u16;
                if p.is_null() {
                    None
                } else {
                    let max = GlobalSize(g) / 2;
                    let mut n = 0;
                    while n < max && *p.add(n) != 0 {
                        n += 1;
                    }
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                    let _ = GlobalUnlock(g);
                    Some(s)
                }
            }
            _ => None,
        }
    };
    unsafe {
        let _ = CloseClipboard();
    }
    r
}

fn numero_area() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

// ---------------------------------------------------------------- microfone do aparelho -> app
// A pagina manda o microfone em PCM 16 bits, 48 kHz, mono (mensagens binarias que comecam por
// 'M'). Aqui toca-se no lado de saida de um cabo virtual (config "microfone", ex.: "CABLE Input");
// a app ouve o outro lado ("CABLE Output") como microfone. Sem o cabo, a pagina nem mostra o botao.

fn dispositivo_saida(nome: &str) -> Option<wasapi::Device> {
    let _ = wasapi::initialize_mta();
    let en = wasapi::DeviceEnumerator::new().ok()?;
    let col = en.get_device_collection(wasapi::Direction::Render).ok()?;
    let n = nome.to_lowercase();
    // preferir o dispositivo normal ao de 16 canais (CABLE In 16 Ch)
    let mut reserva = None;
    for d in &col {
        let Ok(d) = d else { continue };
        let f = d.get_friendlyname().map(|f| f.to_lowercase()).unwrap_or_default();
        if f.contains(&n) {
            if f.contains("16 ch") {
                reserva.get_or_insert(d);
            } else {
                return Some(d);
            }
        }
    }
    reserva
}

fn tem_cabo(nome: &str) -> bool {
    !nome.trim().is_empty() && dispositivo_saida(nome).is_some()
}

/// Fio do microfone: abre o cabo quando chega som e larga-o ao fim de 5 s sem nada.
fn microfone(cabo: String, rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    let _ = wasapi::initialize_mta();
    loop {
        // esperar pelo primeiro bloco
        let Ok(primeiro) = rx.recv() else { return };
        if let Err(e) = tocar_microfone(&cabo, primeiro, &rx) {
            nota(&format!("microfone -> {cabo} falhou: {e}"));
            // deitar fora o que vier durante 3 s para nao encher o registo
            let fim = Instant::now() + Duration::from_secs(3);
            while Instant::now() < fim {
                if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) = rx.recv_timeout(Duration::from_millis(200)) {
                    return;
                }
            }
        }
    }
}

fn tocar_microfone(cabo: &str, primeiro: Vec<u8>, rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
    use wasapi::*;
    let dev = dispositivo_saida(cabo).ok_or("cabo virtual nao encontrado")?;
    let fmt = WaveFormat::new(16, 16, SampleType::Int, 48000, 1, None);
    let mut c = dev.get_iaudioclient()?;
    c.initialize_client(&fmt, Direction::Render, StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 }, None)?;
    let ev = c.set_get_eventhandle()?;
    let render = c.get_audiorenderclient()?;
    let mut fila: std::collections::VecDeque<u8> = primeiro.into();
    const MAX: usize = 48000 * 2 * 3 / 10; // nunca mais de 300 ms de atraso
    c.start_stream()?;
    nota(&format!("microfone ligado -> {cabo}"));
    let mut ultimo = Instant::now();
    loop {
        loop {
            match rx.try_recv() {
                Ok(b) => {
                    fila.extend(b);
                    ultimo = Instant::now();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = c.stop_stream();
                    return Ok(());
                }
            }
        }
        if fila.len() > MAX {
            let a_mais = fila.len() - MAX / 2;
            fila.drain(..a_mais & !1);
        }
        if ultimo.elapsed() > Duration::from_secs(5) {
            break; // microfone desligado na pagina: largar o cabo
        }
        let livre = c.get_available_space_in_frames()? as usize;
        if livre > 0 {
            let n = livre.min(fila.len() / 2);
            if n > 0 {
                render.write_to_device_from_deque(n, &mut fila, None)?;
            }
        }
        let _ = ev.wait_for_event(100);
    }
    let _ = c.stop_stream();
    nota("microfone desligado");
    Ok(())
}

// ---------------------------------------------------------------- camara do aparelho -> app
// A pagina manda a camara em JPEG (mensagens binarias que comecam por 'C'). Aqui escreve-se na
// memoria partilhada da "OBS Virtual Camera" (a mesma fila que o OBS usa, ver
// plugins/win-dshow/shared-memory-queue.c): o OBS so tem de estar instalado, nao aberto. As apps
// do PC escolhem "OBS Virtual Camera" como webcam. Sem o OBS, a pagina nem mostra o botao.

const CAM_L: u32 = 1280;
const CAM_A: u32 = 720;

fn tem_camara() -> bool {
    use windows::Win32::System::Registry::*;
    let mut k = HKEY::default();
    unsafe {
        let r = RegOpenKeyExW(HKEY_CLASSES_ROOT, windows::core::w!("CLSID\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}"), 0, KEY_READ, &mut k);
        if r.is_ok() {
            let _ = RegCloseKey(k);
            true
        } else {
            false
        }
    }
}

/// queue_header do OBS (80 bytes)
#[repr(C)]
struct CabecaFila {
    write_idx: u32,
    read_idx: u32,
    state: u32, // 1 a comecar, 2 pronta, 3 a parar
    offsets: [u32; 3],
    tipo: u32,
    cx: u32,
    cy: u32,
    interval: u64, // 100 ns por imagem
    reserved: [u32; 8],
}

struct FilaCamara {
    h: HANDLE,
    base: *mut u8,
    offsets: [u32; 3],
    inicio: Instant,
}

impl FilaCamara {
    fn abrir() -> Result<Self, String> {
        use windows::Win32::System::Memory::*;
        let nome = windows::core::w!("OBSVirtualCamVideo");
        unsafe {
            // ja existe: o OBS (ou outra pagina) esta a usar a camara virtual, ou a app ainda
            // nao largou a anterior
            if let Ok(h) = OpenFileMappingW(FILE_MAP_READ.0, false, nome) {
                let _ = CloseHandle(h);
                return Err("ocupada".into());
            }
            let al = |x: u32| (x + 31) & !31;
            let quadro = CAM_L * CAM_A * 3 / 2;
            let mut tam = al(std::mem::size_of::<CabecaFila>() as u32);
            let mut offsets = [0u32; 3];
            for o in offsets.iter_mut() {
                *o = tam;
                tam = al(tam + quadro + 32);
            }
            let h = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, tam, nome).map_err(|e| e.to_string())?;
            let v = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if v.Value.is_null() {
                let _ = CloseHandle(h);
                return Err("MapViewOfFile falhou".into());
            }
            let base = v.Value as *mut u8;
            let c = CabecaFila { write_idx: 0, read_idx: 0, state: 1, offsets, tipo: 0, cx: CAM_L, cy: CAM_A, interval: 10_000_000 / 30, reserved: [0; 8] };
            std::ptr::write_volatile(base as *mut CabecaFila, c);
            Ok(FilaCamara { h, base, offsets, inicio: Instant::now() })
        }
    }

    /// uma imagem NV12 de CAM_L x CAM_A
    fn escrever(&mut self, nv12: &[u8]) {
        unsafe {
            let c = self.base as *mut CabecaFila;
            let inc = std::ptr::read_volatile(std::ptr::addr_of!((*c).write_idx)).wrapping_add(1);
            std::ptr::write_volatile(std::ptr::addr_of_mut!((*c).write_idx), inc);
            let off = self.offsets[(inc % 3) as usize] as usize;
            std::ptr::write_volatile(self.base.add(off) as *mut u64, self.inicio.elapsed().as_nanos() as u64);
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), self.base.add(off + 32), nv12.len());
            std::sync::atomic::fence(Ordering::SeqCst);
            std::ptr::write_volatile(std::ptr::addr_of_mut!((*c).read_idx), inc);
            std::ptr::write_volatile(std::ptr::addr_of_mut!((*c).state), 2);
        }
    }
}

impl Drop for FilaCamara {
    fn drop(&mut self) {
        use windows::Win32::System::Memory::*;
        unsafe {
            let c = self.base as *mut CabecaFila;
            std::ptr::write_volatile(std::ptr::addr_of_mut!((*c).state), 3);
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base as *mut c_void });
            let _ = CloseHandle(self.h);
        }
    }
}

/// JPEG (qualquer tamanho) -> NV12 de CAM_L x CAM_A (BT.601, gama limitada)
fn jpeg_para_nv12(jpeg: &[u8], out: &mut Vec<u8>) -> bool {
    let mut d = jpeg_decoder::Decoder::new(jpeg);
    let Ok(px) = d.decode() else { return false };
    let Some(info) = d.info() else { return false };
    let bpp = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => 3,
        jpeg_decoder::PixelFormat::L8 => 1,
        _ => return false,
    };
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || px.len() < w * h * bpp {
        return false;
    }
    let (lw, la) = (CAM_L as usize, CAM_A as usize);
    out.resize(lw * la * 3 / 2, 0);
    let (yp, uv) = out.split_at_mut(lw * la);
    let cor = |x: usize, y: usize| -> (i32, i32, i32) {
        let i = ((y * h / la) * w + x * w / lw) * bpp;
        if bpp == 1 {
            let v = px[i] as i32;
            (v, v, v)
        } else {
            (px[i] as i32, px[i + 1] as i32, px[i + 2] as i32)
        }
    };
    for y in (0..la).step_by(2) {
        for x in (0..lw).step_by(2) {
            let (mut su, mut sv) = (0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (r, g, b) = cor(x + dx, y + dy);
                yp[(y + dy) * lw + x + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                su += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                sv += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            }
            let i = (y / 2) * lw + x;
            uv[i] = (su / 4).clamp(0, 255) as u8;
            uv[i + 1] = (sv / 4).clamp(0, 255) as u8;
        }
    }
    true
}

/// Fio da camara: abre a fila quando chega a primeira imagem e larga-a ao fim de 3 s sem nada
/// (as apps voltam a ver a imagem parada do OBS).
fn camara(rx: std::sync::mpsc::Receiver<Vec<u8>>, tx: tokio::sync::mpsc::Sender<Saida>) {
    let mut nv12 = Vec::new();
    loop {
        let Ok(mut jpeg) = rx.recv() else { return };
        // a app pode ainda estar a largar a fila anterior: tentar durante 3 s
        let fim = Instant::now() + Duration::from_secs(3);
        let fila = loop {
            match FilaCamara::abrir() {
                Ok(f) => break Some(f),
                Err(e) if Instant::now() > fim => {
                    nota(&format!("camara -> OBS Virtual Camera falhou: {e}"));
                    let m = if e == "ocupada" {
                        "A câmara virtual já está a ser usada (o OBS ou outra página do KCU Janelas)."
                    } else {
                        "Não consegui ligar a câmara virtual no PC."
                    };
                    let _ = tx.blocking_send(Saida::Texto(serde_json::json!({ "camaraErro": m }).to_string()));
                    break None;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(300)),
            }
        };
        let Some(mut fila) = fila else {
            // deitar fora o que ainda vier ate a pagina desligar
            while rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
            continue;
        };
        nota("camara ligada -> OBS Virtual Camera");
        loop {
            if jpeg_para_nv12(&jpeg, &mut nv12) {
                fila.escrever(&nv12);
            }
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(b) => {
                    jpeg = b;
                    while let Ok(n) = rx.try_recv() {
                        jpeg = n; // atrasado: so a mais recente
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    drop(fila);
                    nota("camara desligada");
                    return;
                }
            }
        }
        drop(fila);
        nota("camara desligada");
    }
}

// ---------------------------------------------------------------- envio de imagens

/// Codifica e manda as imagens (H.264 pela NVIDIA ou JPEG), igual para uma app e para o ecra.
struct Emissor {
    video: Option<(String, u32)>,
    largura: u32,
    qualidade: u8,
    cod: Option<Codificador>,
    ultimo_px: Option<(u32, u32, Vec<u8>)>, // para repetir quando a imagem esta parada
    alimentado: Instant,
    repeticoes: u32,
    ultima: u64,
    enviada: Instant,
    falhas_h264: u32,
}

impl Emissor {
    fn novo(video: Option<(String, u32)>, largura: u32, qualidade: u8) -> Self {
        Self {
            video,
            largura,
            qualidade,
            cod: None,
            ultimo_px: None,
            alimentado: Instant::now(),
            repeticoes: 0,
            ultima: 0,
            enviada: Instant::now() - Duration::from_secs(10),
            falhas_h264: 0,
        }
    }

    /// Manda a imagem (se houver). false = a pagina fechou.
    fn passo(&mut self, imagem: Option<(u32, u32, Vec<u8>)>, tx: &tokio::sync::mpsc::Sender<Saida>) -> bool {
        let imagem = imagem.filter(|(w, hh, px)| *w > 0 && *hh > 0 && *w <= 16000 && *hh <= 16000 && px.len() == (*w * *hh * 4) as usize);
        if let Some((vf, kbps)) = self.video.as_ref() {
            // ---- video H.264: cada imagem nova vai ao ffmpeg; parada, repete-se a cada 33 ms
            let largura = self.largura;
            let nova = imagem.map(|(w, hh, px)| {
                let (w, hh, px) = reduzir(w, hh, px, if largura == 0 { 3840 } else { largura.min(3840) });
                pares(w, hh, px)
            });
            // parada: repetir a ultima imagem so umas vezes (o ffmpeg so solta uma imagem quando
            // chega a seguinte) e depois de 2 em 2 s, para nao gastar CPU a toa
            let quadro_h = match nova {
                Some(q) => {
                    self.repeticoes = 0;
                    Some(q)
                }
                None if (self.repeticoes < 4 && self.alimentado.elapsed() > Duration::from_millis(33))
                    || self.alimentado.elapsed() > Duration::from_secs(2) =>
                {
                    self.repeticoes += 1;
                    self.ultimo_px.take()
                }
                None => None,
            };
            if let Some((w, hh, px)) = quadro_h {
                if self.cod.as_ref().map(|c| c.w != w || c.h != hh).unwrap_or(true) {
                    self.cod = Codificador::novo(vf, w, hh, *kbps, tx.clone());
                }
                let ok = self.cod.as_mut().map(|c| c.imagem(px.clone())).unwrap_or(false);
                if !ok {
                    // morreu sem nunca dar uma imagem: nao adianta relancar a cada imagem
                    if self.cod.as_ref().map(|c| c.saiu.load(std::sync::atomic::Ordering::Relaxed) == 0).unwrap_or(true) {
                        self.falhas_h264 += 1;
                    }
                    self.cod = None;
                }
                self.alimentado = Instant::now();
                self.enviada = Instant::now();
                self.ultimo_px = Some((w, hh, px));
            }
            // o ffmpeg/NVENC as vezes encrava sem soltar nada (a pagina ficava em "A ligar ao PC"
            // para sempre): passa-se esta ligacao para JPEG, que funciona sempre
            if self.cod.as_ref().map(|c| c.encravado()).unwrap_or(false) || self.falhas_h264 >= 3 {
                nota("H.264 sem imagens (ffmpeg encravado ou a morrer): a passar esta ligacao para JPEG");
                self.cod = None;
                self.video = None;
                self.enviada = Instant::now() - Duration::from_secs(10);
                let _ = tx.blocking_send(Saida::Texto("{\"video\":\"jpeg\"}".into()));
            }
            !tx.is_closed()
        } else if let Some((w, hh, px)) = imagem {
            let (w, hh, px) = reduzir(w, hh, px, self.largura);
            let sig = assinatura(&px);
            if sig != self.ultima || self.enviada.elapsed() > Duration::from_secs(3) {
                let mut out = Vec::new();
                let enc = jpeg_encoder::Encoder::new(&mut out, self.qualidade);
                match enc.encode(&px, w as u16, hh as u16, jpeg_encoder::ColorType::Bgra) {
                    Ok(_) => {
                        if tx.blocking_send(Saida::Jpeg(out)).is_err() {
                            return false;
                        }
                        self.ultima = sig;
                        self.enviada = Instant::now();
                    }
                    Err(e) => {
                        if self.ultima == 0 {
                            nota(&format!("JPEG falhou {w}x{hh}: {e}"));
                            self.ultima = 1;
                        }
                    }
                }
            }
            true
        } else {
            true
        }
    }
}

/// Copiar: texto novo na area de transferencia do PC -> pagina. false = a pagina fechou.
fn area_para_pagina(area_n: &mut u32, area_em: &mut Instant, tx: &tokio::sync::mpsc::Sender<Saida>) -> bool {
    if area_em.elapsed() > Duration::from_millis(300) {
        *area_em = Instant::now();
        let n = numero_area();
        if n != *area_n {
            *area_n = n;
            if n != AREA_NOSSA.load(Ordering::Relaxed) {
                if let Some(s) = ler_area().filter(|s| !s.is_empty() && s.len() <= 200_000) {
                    let m = serde_json::json!({ "clip": s.replace("\r\n", "\n") }).to_string();
                    if tx.blocking_send(Saida::Texto(m)).is_err() {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Fio de captura do ecra inteiro (monitor principal): WGC, ou BitBlt se o WGC falhar.
fn ciclo_ecra(tx: tokio::sync::mpsc::Sender<Saida>, parar: Arc<Mutex<bool>>, cmd_rx: std::sync::mpsc::Receiver<String>, mut em: Emissor) {
    nota("ecra inteiro: sessao aberta");
    let quadro: Quadro = Arc::new(Mutex::new(None));
    let mut wgc = iniciar_wgc_ecra(quadro.clone());
    let mut visto = 0u64;
    let mut parado_desde = Instant::now();
    let mut intervalo = Duration::from_millis(16);
    let mut area_n = numero_area();
    let mut area_em = Instant::now();
    let mut primeira = true;
    let mut no_seguro = false;
    loop {
        if parado(&parar) || tx.is_closed() {
            break;
        }
        let t0 = Instant::now();
        if !area_para_pagina(&mut area_n, &mut area_em, &tx) {
            break;
        }
        while let Ok(c) = cmd_rx.try_recv() {
            if c == "key" {
                em.cod = None;
            } else if let Some(n) = c.strip_prefix("fps:").and_then(|n| n.parse::<u64>().ok()) {
                intervalo = Duration::from_millis(1000 / n.clamp(5, 60));
            }
        }
        let seguro = ambiente_seguro();
        if seguro != no_seguro {
            no_seguro = seguro;
            nota(if seguro { "ecra inteiro: ambiente seguro (UAC/bloqueio): imagens pelo ajudante" } else { "ecra inteiro: de volta ao ambiente normal" });
        }
        let mut imagem = None;
        if seguro {
            imagem = imagem_segura();
        } else if wgc.is_some() {
            let q = quadro.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match q {
                Some((w, hh, px, n)) => {
                    if n != visto {
                        visto = n;
                        parado_desde = Instant::now();
                        imagem = Some((w, hh, px));
                    } else if em.enviada.elapsed() > Duration::from_secs(3) {
                        imagem = Some((w, hh, px)); // ecra parado: reenviar de vez em quando
                    }
                }
                None if parado_desde.elapsed() > Duration::from_secs(4) => {
                    nota("WGC do ecra sem imagens ao fim de 4 s; a usar BitBlt");
                    if let Some(c) = wgc.take() {
                        let _ = c.stop();
                    }
                }
                None => {}
            }
        } else {
            imagem = capturar_ecra();
        }
        if primeira {
            if let Some((w, hh, _)) = imagem.as_ref() {
                primeira = false;
                nota(&format!("ecra inteiro: primeira imagem {w}x{hh} ({})", if wgc.is_some() { "WGC" } else { "BitBlt" }));
            }
        }
        if !em.passo(imagem, &tx) {
            break;
        }
        // no ambiente seguro cada imagem atravessa o ajudante em bruto: 10/s chegam para um pedido de admin
        let alvo = if seguro { intervalo.max(Duration::from_millis(100)) } else { intervalo };
        let gasto = t0.elapsed();
        if gasto < alvo {
            std::thread::sleep(alvo - gasto);
        }
    }
    if let Some(c) = wgc.take() {
        let _ = c.stop();
    }
    nota("ecra inteiro: sessao fechada");
}

// ---------------------------------------------------------------- ambiente seguro (pedidos de admin, ecra de bloqueio)
//
// O Windows mostra o UAC, o ecra de bloqueio e o Ctrl+Alt+Del no ambiente "Winlogon", onde so a
// conta SYSTEM entra (como fazem o Chrome Remote Desktop e o RustDesk instalados):
// - `--guarda [config]`: tarefa SYSTEM no arranque (sessao 0). Lanca `--seguro` na sessao da
//   consola com o proprio token SYSTEM e relanca-o se morrer ou se a sessao mudar.
// - `--seguro`: o ajudante. So escuta em 127.0.0.1:PORTA_SEGURO, pede a chave, e SO mostra/mexe
//   enquanto o ambiente de entrada NAO for o normal ("Default").
// - este processo (o do utilizador, modo ecra) pede-lhe as imagens e passa-lhe o rato/teclado.

use windows::Win32::System::StationsAndDesktops::*;

const PORTA_SEGURO: u16 = 3096;
static CHAVE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static AJUDANTE: Mutex<Option<std::net::TcpStream>> = Mutex::new(None);

fn nome_do_ambiente(d: HDESK) -> Option<String> {
    let mut b = [0u16; 64];
    let mut n = 0u32;
    unsafe {
        GetUserObjectInformationW(HANDLE(d.0), UOI_NAME, Some(b.as_mut_ptr() as *mut c_void), (b.len() * 2) as u32, Some(&mut n)).ok()?;
    }
    let s = String::from_utf16_lossy(&b[..(n as usize / 2).min(b.len())]);
    Some(s.trim_end_matches('\0').to_string())
}

/// O ambiente que recebe o rato/teclado nao e o normal. Do lado do utilizador, o Winlogon nem abre.
fn ambiente_seguro() -> bool {
    unsafe {
        let Ok(d) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) else { return true };
        let n = nome_do_ambiente(d);
        let _ = CloseDesktop(d);
        n.map_or(true, |n| !n.eq_ignore_ascii_case("default"))
    }
}

/// Um pedido ao ajudante: 'F' devolve uma imagem (w, h, BGRA); 'I' + evento JSON nao devolve nada.
/// Uma ligacao so, reaberta se cair; sem ajudante (tarefa nao instalada) devolve None depressa.
fn ao_ajudante(tipo: u8, dados: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut g = AJUDANTE.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..2 {
        if g.is_none() {
            let mut s = std::net::TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, PORTA_SEGURO).into(), Duration::from_millis(300)).ok()?;
            let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
            let _ = s.set_nodelay(true);
            let c = CHAVE.get()?;
            s.write_all(&(c.len() as u32).to_le_bytes()).ok()?;
            s.write_all(c.as_bytes()).ok()?;
            *g = Some(s);
        }
        let s = g.as_mut()?;
        let r = (|| -> std::io::Result<Vec<u8>> {
            s.write_all(&[tipo])?;
            s.write_all(&(dados.len() as u32).to_le_bytes())?;
            s.write_all(dados)?;
            if tipo != b'F' {
                return Ok(Vec::new());
            }
            let mut n = [0u8; 4];
            s.read_exact(&mut n)?;
            let mut v = vec![0u8; u32::from_le_bytes(n) as usize];
            s.read_exact(&mut v)?;
            Ok(v)
        })();
        match r {
            Ok(v) => return Some(v),
            Err(_) => *g = None,
        }
    }
    None
}

fn imagem_segura() -> Option<(u32, u32, Vec<u8>)> {
    let mut v = ao_ajudante(b'F', &[])?;
    if v.len() < 8 {
        return None; // o ajudante recusou (ambiente normal) ou nao conseguiu copiar
    }
    let w = u32::from_le_bytes(v[0..4].try_into().ok()?);
    let h = u32::from_le_bytes(v[4..8].try_into().ok()?);
    let px = v.split_off(8);
    (px.len() as u64 == w as u64 * h as u64 * 4).then_some((w, h, px))
}

/// Liga este fio ao ambiente de entrada atual (fecha o anterior) e devolve o nome dele.
fn entrar_no_ambiente(atual: &mut Option<HDESK>) -> Option<String> {
    unsafe {
        let d = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x1000_0000)).ok()?; // GENERIC_ALL
        if SetThreadDesktop(d).is_err() {
            let _ = CloseDesktop(d);
            return None;
        }
        if let Some(v) = atual.replace(d) {
            let _ = CloseDesktop(v);
        }
        nome_do_ambiente(d)
    }
}

fn ler_bloco(s: &mut std::net::TcpStream, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut n = [0u8; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_le_bytes(n) as usize;
    if n > max {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut v = vec![0u8; n];
    s.read_exact(&mut v)?;
    Ok(v)
}

fn atender_seguro(mut s: std::net::TcpStream, chave: &str) -> std::io::Result<()> {
    use std::io::Read;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    if ler_bloco(&mut s, 256)? != chave.as_bytes() {
        nota("ajudante seguro: chave errada; ligacao recusada");
        return Ok(());
    }
    s.set_read_timeout(None)?;
    s.set_nodelay(true)?;
    let mut atual = None;
    loop {
        let mut t = [0u8; 1];
        s.read_exact(&mut t)?;
        let dados = ler_bloco(&mut s, 64 * 1024)?;
        let seguro = entrar_no_ambiente(&mut atual).is_some_and(|n| !n.eq_ignore_ascii_case("default"));
        match t[0] {
            b'F' => {
                let mut v = Vec::new();
                if let Some((w, h, px)) = capturar_ecra().filter(|_| seguro) {
                    v.extend_from_slice(&w.to_le_bytes());
                    v.extend_from_slice(&h.to_le_bytes());
                    v.extend_from_slice(&px);
                }
                s.write_all(&(v.len() as u32).to_le_bytes())?;
                s.write_all(&v)?;
            }
            b'I' if seguro => {
                // so rato, teclas e texto: colar ("v") mexe na area de transferencia, que aqui nao existe
                if let Ok(ev) = serde_json::from_slice::<Ev>(&dados) {
                    if matches!(ev.t.as_str(), "m" | "k" | "c") {
                        aplicar(HWND::default(), true, &ev);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `--seguro`: corre como SYSTEM na sessao do utilizador (lancado pela guarda).
fn ajudante(chave: String) {
    let l = match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, PORTA_SEGURO)) {
        Ok(l) => l,
        Err(e) => {
            nota(&format!("ajudante seguro: porta {PORTA_SEGURO} ocupada ({e}); a sair"));
            return;
        }
    };
    nota(&format!("ajudante seguro: a escutar em 127.0.0.1:{PORTA_SEGURO}"));
    for s in l.incoming().flatten() {
        let c = chave.clone();
        std::thread::spawn(move || {
            let _ = atender_seguro(s, &c);
        });
    }
}

/// Lanca `<este exe> --seguro` na `sessao` com uma copia do token SYSTEM desta guarda.
fn lancar_seguro(sessao: u32) -> Option<HANDLE> {
    use windows::Win32::Security::*;
    use windows::Win32::System::Threading::*;
    let exe = std::env::current_exe().ok()?;
    let mut linha: Vec<u16> = format!("\"{}\" --seguro", exe.display()).encode_utf16().chain([0]).collect();
    let mut mesa: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    unsafe {
        let mut eu = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut eu).ok()?;
        let mut t = HANDLE::default();
        let r = DuplicateTokenEx(eu, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut t);
        let _ = CloseHandle(eu);
        r.ok()?;
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: windows::core::PWSTR(mesa.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let ok = SetTokenInformation(t, TokenSessionId, &sessao as *const u32 as *const c_void, 4).is_ok()
            && CreateProcessAsUserW(t, windows::core::PCWSTR::null(), windows::core::PWSTR(linha.as_mut_ptr()), None, None, false, PROCESS_CREATION_FLAGS(0), None, windows::core::PCWSTR::null(), &si, &mut pi).is_ok();
        let _ = CloseHandle(t);
        if !ok {
            nota(&format!("guarda: nao consegui lancar o ajudante na sessao {sessao}: {}", windows::core::Error::from_win32()));
            return None;
        }
        let _ = CloseHandle(pi.hThread);
        Some(pi.hProcess)
    }
}

/// `--guarda`: mantem um ajudante vivo na sessao da consola.
fn guarda() {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    nota("guarda do ambiente seguro: a comecar");
    let mut filho: Option<(u32, HANDLE)> = None;
    let mut falhas = 0u32;
    loop {
        let sessao = unsafe { WTSGetActiveConsoleSessionId() };
        let vivo = filho.as_ref().is_some_and(|(s, h)| *s == sessao && unsafe { WaitForSingleObject(*h, 0) } == WAIT_TIMEOUT);
        if vivo {
            falhas = 0;
        } else {
            falhas += 1;
            if let Some((_, h)) = filho.take() {
                unsafe {
                    let _ = TerminateProcess(h, 0);
                    let _ = CloseHandle(h);
                }
            }
            if sessao != u32::MAX && sessao != 0 {
                filho = lancar_seguro(sessao).map(|h| (sessao, h));
            }
        }
        // se o ajudante morre logo (ex.: porta ocupada), nao o relancar 30 vezes por minuto
        std::thread::sleep(Duration::from_secs(if falhas > 3 { 60 } else { 2 }));
    }
}

// ---------------------------------------------------------------- sessao

enum Saida {
    Jpeg(Vec<u8>),
    H264(Vec<u8>),
    Texto(String),
}

async fn sessao(s: WebSocket, a: App, largura: u32, qualidade: u8, video: Option<(String, u32)>, cabo: String, cam: bool) {
    let (mut tx_ws, mut rx_ws) = s.split();
    let atual = Arc::new(AtomicIsize::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Saida>(2);
    let ecra = a.processo.is_empty(); // ecra inteiro: nada para abrir nem fechar

    // abrir a app se nao estiver aberta
    if !ecra && pids_de(&a.processo).is_empty() {
        match lancar(&a) {
            Ok(_) => nota(&format!("abri {}", a.nome)),
            Err(e) => {
                let _ = tx_ws.send(Message::Text(format!("{{\"erro\":\"Nao consegui abrir {}: {}\"}}", a.nome, e))).await;
                return;
            }
        }
    }

    if !ecra {
        entrou(&a.processo); // conta paginas abertas desta app (para a fechar quando todas sairem)
    }
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();

    // fio de captura
    let a2 = a.clone();
    let atual2 = atual.clone();
    let parar = Arc::new(Mutex::new(false));
    let parar2 = parar.clone();

    // fio do som (so o desta app; desligado ate a pagina o pedir)
    let som_ligado = Arc::new(AtomicBool::new(false));
    let (som_tx, mut som_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    {
        let (pr, p, l) = (a.processo.clone(), parar.clone(), som_ligado.clone());
        std::thread::spawn(move || som(pr, p, l, som_tx));
    }
    // microfone do aparelho (o fio so arranca quando chega o primeiro bloco)
    let (mic_tx, mic_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut mic_rx = Some(mic_rx);
    // camara do aparelho (o fio so arranca quando chega a primeira imagem)
    let (cam_tx, cam_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut cam_rx = Some(cam_rx);
    let tx_cam = tx.clone();
    let cabo2 = cabo.clone();
    let parar3 = parar.clone();
    std::thread::spawn(move || {
        let mut em = Emissor::novo(video, largura, qualidade);
        let mut procura = Instant::now() - Duration::from_secs(10);
        let mut sem_janela_desde = Instant::now();
        let inicio = Instant::now();
        let mut h = 0isize;
        let mut diag_feito = false;
        let quadro: Quadro = Arc::new(Mutex::new(None));
        let mut wgc: Option<ControloWgc> = None;
        let mut wgc_de = 0isize; // janela que o WGC esta a capturar
        let mut visto = 0u64;
        let mut wgc_parado_desde = Instant::now();
        // login pelo browser
        let antes: Vec<isize> = janelas_de_pids(pids_browsers()); // janelas de browser que ja existiam
        let mut browsers: Vec<u32> = pids_browsers();
        let mut browsers_em = Instant::now();
        let mut login = 0isize; // janela de browser do login
        let mut no_browser = false; // a mostrar o browser em vez da app
        let mut novas: Vec<isize> = Vec::new(); // janelas de browser abertas durante a sessao (fechar no fim)
        let mut fg_antes = 0isize;
        let mut ignorar = 0isize; // browser de onde se pediu para voltar a app
        let mut intervalo = Duration::from_millis(16); // 60 img/s; a pagina pode pedir menos
        if em.video.is_some() {
            let _ = tx.blocking_send(Saida::Texto("{\"video\":\"h264\"}".into()));
        } else {
            let _ = tx.blocking_send(Saida::Texto("{\"video\":\"jpeg\"}".into()));
        }
        {
            // noutro fio: o COM do audio nao se mistura com o da captura de imagem
            let (t, c) = (tx.clone(), cabo2.clone());
            std::thread::spawn(move || {
                let _ = t.blocking_send(Saida::Texto(format!("{{\"microfone\":{}}}", tem_cabo(&c))));
                let _ = t.blocking_send(Saida::Texto(format!("{{\"camara\":{}}}", cam && tem_camara())));
            });
        }
        if ecra {
            ciclo_ecra(tx, parar3, cmd_rx, em);
            return;
        }
        let mut area_n = numero_area(); // o que ja estava copiado antes da sessao nao se manda
        let mut area_em = Instant::now();
        let mut maximizadas: Vec<isize> = Vec::new(); // janelas da app ja maximizadas nesta sessao
        let mut relancada = false;
        let mut ja_vista = false; // a app ja mostrou janela nesta sessao
        let mut fechada_avisada = false;
        let mut sumiu_em: Option<Instant> = None;
        loop {
            if *parar2.lock().unwrap() || tx.is_closed() {
                break;
            }
            let t0 = Instant::now();
            // copiar: texto novo na area de transferencia do PC -> pagina
            if !area_para_pagina(&mut area_n, &mut area_em, &tx) {
                break;
            }
            if h == 0 || !unsafe { IsWindow(hwnd(h)).as_bool() } || procura.elapsed() > Duration::from_millis(500) {
                let n = janela_de(&a2.processo);
                if n != 0 && n != h {
                    unsafe {
                        if IsIconic(hwnd(n)).as_bool() {
                            let _ = ShowWindow(hwnd(n), SW_RESTORE);
                        }
                    }
                }
                h = n;
                // sempre em ecra inteiro: maximizar cada janela da app uma vez por sessao
                if h != 0 && !maximizadas.contains(&h) {
                    maximizadas.push(h);
                    unsafe {
                        let _ = ShowWindow(hwnd(h), SW_MAXIMIZE);
                    }
                }
                atual2.store(h, Ordering::Relaxed);
                procura = Instant::now();
            }
            if h != 0 {
                ja_vista = true;
                fechada_avisada = false;
                sumiu_em = None;
            }
            if h == 0 && ja_vista {
                // fechou-se a app pelo X na pagina: avisar e, se ficar escondida a correr, termina-la
                let desde = *sumiu_em.get_or_insert_with(Instant::now);
                if !fechada_avisada && desde.elapsed() > Duration::from_secs(3) {
                    fechada_avisada = true;
                    nota(&format!("{}: a janela foi fechada na pagina", a2.nome));
                    if a2.fechar {
                        terminar_escondida(&a2.processo, &a2.nome);
                    }
                    let _ = tx.blocking_send(Saida::Texto("{\"estado\":\"A app foi fechada.\",\"fechada\":true}".into()));
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            if h == 0 {
                if sem_janela_desde.elapsed() > Duration::from_secs(1) {
                    let msg = if inicio.elapsed() < Duration::from_secs(45) { "A abrir a app..." } else { "A app nao tem janela aberta." };
                    // a correr mas sem janela (escondida na bandeja): abrir outra vez mostra a janela
                    if !relancada && !ja_vista && inicio.elapsed() > Duration::from_secs(4) && inicio.elapsed() < Duration::from_secs(15)
                        && !pids_de(&a2.processo).is_empty()
                    {
                        relancada = true;
                        if lancar(&a2).is_ok() {
                            nota(&format!("{} a correr sem janela: abri outra vez para a mostrar", a2.nome));
                        }
                    }
                    if !diag_feito && inicio.elapsed() > Duration::from_secs(6) {
                        diagnostico(&a2.processo);
                        diag_feito = true;
                    }
                    // a janela existe mas esta invisivel (bandeja): mostra-la; a proxima volta encontra-a
                    if !ja_vista && inicio.elapsed() > Duration::from_secs(6) && inicio.elapsed() < Duration::from_secs(45)
                        && mostrar_escondida(&a2.processo) != 0
                    {
                        nota(&format!("{} escondida na bandeja: mostrei a janela dela", a2.nome));
                    }
                    if tx.blocking_send(Saida::Texto(format!("{{\"estado\":\"{msg}\"}}"))).is_err() {
                        break;
                    }
                    sem_janela_desde = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            unsafe {
                if IsIconic(hwnd(h)).as_bool() {
                    let _ = ShowWindow(hwnd(h), SW_RESTORE);
                }
            }
            // ---- login pelo browser: escolher o alvo (app ou browser)
            let fg = unsafe { GetForegroundWindow() }.0 as isize;
            // lista de browsers: atualizar quando a janela da frente muda (um browser acabado de abrir)
            if fg != fg_antes || browsers_em.elapsed() > Duration::from_secs(3) {
                browsers = pids_browsers();
                browsers_em = Instant::now();
            }
            let mut avisar = false;
            while let Ok(c) = cmd_rx.try_recv() {
                if c == "key" {
                    em.cod = None; // o proximo codificador comeca com SPS/PPS + imagem completa
                    continue;
                }
                if let Some(n) = c.strip_prefix("fps:").and_then(|n| n.parse::<u64>().ok()) {
                    intervalo = Duration::from_millis(1000 / n.clamp(5, 60));
                    continue;
                }
                no_browser = c == "browser" && login != 0 && unsafe { IsWindow(hwnd(login)).as_bool() };
                if !no_browser {
                    ignorar = login; // pediu para voltar a app: nao saltar logo outra vez para este browser
                }
                avisar = true;
            }
            if fg != ignorar && ignorar != 0 && fg != fg_antes {
                ignorar = 0;
            }
            if !no_browser
                && fg != 0
                && fg != h
                && fg != ignorar
                && inicio.elapsed() > Duration::from_secs(2)
                && browsers.contains(&pid_da_janela(hwnd(fg)))
                && unsafe { IsWindowVisible(hwnd(fg)).as_bool() }
            {
                login = fg;
                no_browser = true;
                avisar = true;
                if !antes.contains(&fg) && !novas.contains(&fg) {
                    novas.push(fg);
                }
                nota(&format!("{}: browser veio para a frente (login?) - a mostrar o browser", a2.nome));
            }
            if no_browser {
                let morta = login == 0 || unsafe { !IsWindow(hwnd(login)).as_bool() || !IsWindowVisible(hwnd(login)).as_bool() };
                let app_voltou = fg == h && fg_antes != h;
                if morta || app_voltou {
                    no_browser = false;
                    avisar = true;
                    ignorar = login;
                    if app_voltou && novas.contains(&login) {
                        fechar_janela(login); // login feito: fechar o browser que abriu para isto
                        nota(&format!("{}: app voltou a frente - fechei o browser do login", a2.nome));
                    }
                }
            }
            fg_antes = fg;
            let login_viva = login != 0 && unsafe { IsWindow(hwnd(login)).as_bool() };
            if avisar {
                let m = format!(
                    "{{\"modo\":\"{}\",\"login\":{}}}",
                    if no_browser { "browser" } else { "app" },
                    login_viva
                );
                if tx.blocking_send(Saida::Texto(m)).is_err() {
                    break;
                }
            }
            let alvo = if no_browser { login } else { h };
            atual2.store(alvo, Ordering::Relaxed);
            let h = alvo; // daqui para baixo captura-se o alvo
            // (re)ligar o WGC quando a janela muda
            if wgc_de != h {
                if let Some(c) = wgc.take() {
                    let _ = c.stop();
                }
                *quadro.lock().unwrap_or_else(|e| e.into_inner()) = None;
                wgc = iniciar_wgc(h, quadro.clone());
                wgc_de = h;
                visto = 0;
                wgc_parado_desde = Instant::now();
            }
            let mut imagem = None;
            if wgc.is_some() {
                let q = quadro.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match q {
                    Some((w, hh, px, n)) => {
                        if n != visto {
                            visto = n;
                            wgc_parado_desde = Instant::now();
                            imagem = Some((w, hh, px));
                        } else if em.enviada.elapsed() > Duration::from_secs(3) {
                            imagem = Some((w, hh, px)); // reenviar de vez em quando (janela parada)
                        }
                    }
                    None if wgc_parado_desde.elapsed() > Duration::from_secs(4) => {
                        nota("WGC sem imagens ao fim de 4 s; a usar PrintWindow");
                        if let Some(c) = wgc.take() {
                            let _ = c.stop();
                        }
                    }
                    None => {}
                }
            } else {
                imagem = capturar(hwnd(h));
            }
            if !em.passo(imagem, &tx) {
                break;
            }
            let gasto = t0.elapsed();
            if gasto < intervalo {
                std::thread::sleep(intervalo - gasto); // ~60 imagens/s no maximo (menos se a pagina pedir)
            }
        }
        if let Some(c) = wgc.take() {
            let _ = c.stop();
        }
        for j in novas {
            fechar_janela(j);
        }
    });

    // enviar imagens
    let envio = tokio::spawn(async move {
        let mut som_vivo = true;
        loop {
            let r = tokio::select! {
                m = rx.recv() => match m {
                    Some(Saida::Jpeg(b)) | Some(Saida::H264(b)) => tx_ws.send(Message::Binary(b)).await,
                    Some(Saida::Texto(t)) => tx_ws.send(Message::Text(t)).await,
                    None => break,
                },
                s = som_rx.recv(), if som_vivo => match s {
                    Some(b) => tx_ws.send(Message::Binary(b)).await,
                    None => { som_vivo = false; Ok(()) }
                },
            };
            if r.is_err() {
                break;
            }
        }
    });

    // receber rato/teclado
    let mut saiu_de_proposito = false; // a pagina avisou que o separador fechou
    while let Some(Ok(m)) = rx_ws.next().await {
        if let Message::Text(t) = m {
            if let Ok(ev) = serde_json::from_str::<Ev>(&t) {
                if ev.t == "alvo" {
                    let _ = cmd_tx.send(ev.a.clone());
                    continue;
                }
                if ev.t == "key" {
                    let _ = cmd_tx.send("key".into());
                    continue;
                }
                if ev.t == "sair" {
                    saiu_de_proposito = true;
                    continue;
                }
                if ev.t == "som" {
                    som_ligado.store(ev.d != 0, Ordering::Relaxed);
                    continue;
                }
                if ev.t == "fps" {
                    let _ = cmd_tx.send(format!("fps:{}", ev.d));
                    continue;
                }
                if ecra && ambiente_seguro() {
                    let _ = ao_ajudante(b'I', t.as_bytes());
                    continue;
                }
                let h = atual.load(Ordering::Relaxed);
                aplicar(hwnd(h), ecra, &ev);
            }
        } else if let Message::Binary(b) = m {
            if b.first() == Some(&b'M') && b.len() > 1 {
                if let Some(r) = mic_rx.take() {
                    let c = cabo.clone();
                    std::thread::spawn(move || microfone(c, r));
                }
                let _ = mic_tx.send(b[1..].to_vec());
            } else if b.first() == Some(&b'C') && b.len() > 1 {
                if let Some(r) = cam_rx.take() {
                    let t = tx_cam.clone();
                    std::thread::spawn(move || camara(r, t));
                }
                let _ = cam_tx.send(b[1..].to_vec());
            }
        } else if let Message::Close(_) = m {
            break;
        }
    }
    drop(mic_tx);
    drop(cam_tx);
    drop(tx_cam);
    *parar.lock().unwrap() = true;
    envio.abort();

    // ultima pagina desta app fechada: esperar e, se ninguem voltar, fechar a app no PC
    if ecra {
        return;
    }
    if let Some(geracao) = saiu(&a.processo).filter(|_| a.fechar) {
        // separador fechado de proposito: 3 s (so para um recarregar nao fechar); senao 45 s
        tokio::time::sleep(if saiu_de_proposito { Duration::from_secs(3) } else { FECHO_APOS }).await;
        if ainda_sozinha(&a.processo, geracao) {
            let (pr, nome) = (a.processo.clone(), a.nome.clone());
            let _ = tokio::task::spawn_blocking(move || fechar_app(&pr, &nome)).await;
        }
    }
}

const PAGINA: &str = include_str!("pagina.html");
