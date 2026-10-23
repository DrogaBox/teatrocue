//! Guion técnico (cue sheet): la estructura completa de la obra como una página HTML imprimible.
//!
//! El archivo es autocontenido (CSS y un poco de JS embebidos, sin recursos externos) para poder abrirlo en cualquier PC
//! del teatro e imprimirlo en los ensayos. Al imprimir sale en blanco y negro, con la cabecera del Ballet y sin partir las filas.

use std::collections::HashMap;

use crate::cue_dto::{ContinueMode, MemoryMark};
use crate::show_format::{CueBody, CueNodeDto, GroupModeDto, ShowFile};

pub const COMPANY: &str = "Ballet Oficial de Córdoba";
pub const VENUE: &str = "Teatro del Libertador San Martín";

fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            c => c.to_string(),
        })
        .collect()
}

/// `mm:ss.mmm` (o `h:mm:ss` desde la hora) para los tiempos de la planilla; `—` si no hay.
pub fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return String::new();
    }
    let ms = (secs * 1000.0).round() as u64;
    let (m, s, milli) = ((ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000);
    let h = ms / 3_600_000;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else if milli == 0 {
        format!("{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}.{milli:03}")
    }
}

fn kind_label(n: &CueNodeDto) -> &'static str {
    match &n.body {
        CueBody::Audio(_) => "Audio",
        CueBody::Video(_) => "Vídeo",
        CueBody::Fade(_) => "Fade",
        CueBody::Control(_) => "Control",
        CueBody::Osc(_) => "OSC",
        CueBody::Memo => "Memo",
        CueBody::Group(_) => "Grupo",
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn memories_of(n: &CueNodeDto) -> &[MemoryMark] {
    match &n.body {
        CueBody::Audio(a) => &a.time_loops.memories,
        CueBody::Video(v) => &v.audio.time_loops.memories,
        _ => &[],
    }
}

/// `durations`: duración conocida por cue (el motor sabe cuánto dura cada archivo); si falta, se usa lo que diga el show.
pub fn render_html(show: &ShowFile, durations: &HashMap<u64, f64>, generated: &str) -> String {
    let mut numbers: HashMap<u64, String> = HashMap::new();
    show.walk(|n, _| {
        numbers.insert(n.basics.unique_id.0, n.basics.number.clone());
    });
    let mut rows: Vec<String> = Vec::new();
    fn visit(nodes: &[CueNodeDto], depth: usize, numbers: &HashMap<u64, String>, durations: &HashMap<u64, f64>, rows: &mut Vec<String>) {
        for n in nodes {
            let b = &n.basics;
            let name = if b.name.is_empty() { b.default_name.as_str() } else { b.name.as_str() };
            let dest = match &n.body {
                CueBody::Audio(_) | CueBody::Video(_) => b.file_target.as_ref().map(|t| file_name(&t.absolute_path).to_string()).unwrap_or_default(),
                CueBody::Osc(o) => format!("{} {}", o.destination, o.message).trim().to_string(),
                CueBody::Fade(_) | CueBody::Control(_) => b.cue_target_id.map(|id| format!("→ cue {}", numbers.get(&id.0).map_or("?".to_string(), Clone::clone))).unwrap_or_default(),
                CueBody::Memo | CueBody::Group(_) => String::new(),
            };
            let mut kind = kind_label(n).to_string();
            match &n.body {
                CueBody::Control(c) => kind = format!("Control · {}", c.action.label()),
                CueBody::Group(g) => {
                    kind = format!(
                        "Grupo · {}",
                        match g.mode {
                            GroupModeDto::Timeline => "línea de tiempo",
                            GroupModeDto::FireFirstGoToNext => "primera y sigue",
                            GroupModeDto::FireAllSimultaneously => "todas a la vez",
                        }
                    )
                }
                _ => {}
            }
            let duration = durations.get(&b.unique_id.0).copied().filter(|d| *d > 0.0).or(Some(b.action_duration).filter(|d| *d > 0.0)).map(fmt_time).unwrap_or_default();
            let sig = match b.continue_mode {
                ContinueMode::DoNotContinue => "",
                ContinueMode::AutoContinue => "Auto-continue",
                ContinueMode::AutoFollow => "Auto-follow",
            };
            let memories: String = memories_of(n)
                .iter()
                .map(|m| {
                    let note = if m.note.is_empty() { String::new() } else { format!(" — {}", esc(&m.note)) };
                    format!("<li><b>{}</b> {}{note}</li>", fmt_time(m.secs), esc(if m.name.is_empty() { "Memoria" } else { &m.name }))
                })
                .collect();
            let memories = if memories.is_empty() { String::new() } else { format!("<ul class=\"mem\">{memories}</ul>") };
            let mut classes = String::from("cue");
            if matches!(n.body, CueBody::Group(_)) {
                classes.push_str(" group");
            }
            if !b.armed {
                classes.push_str(" off");
            }
            let flag = if b.flagged { " ⚑" } else { "" };
            let search = esc(&format!("{} {} {} {} {}", b.number, name, dest, kind, b.notes).to_lowercase());
            let html = format!(
                "<tr class=\"{classes}\" data-s=\"{search}\"><td class=\"n\">{}</td><td class=\"nm\" style=\"padding-left:{}px\">{}{flag}</td><td>{}</td><td>{}</td><td class=\"t\">{}</td><td class=\"t\">{}</td><td>{sig}</td><td>{memories}</td><td class=\"nt\">{}</td></tr>",
                esc(&b.number),
                8 + depth * 18,
                esc(name),
                esc(&dest),
                esc(&kind),
                fmt_time(b.pre_wait),
                duration,
                esc(&b.notes).replace('\n', "<br>"),
            );
            rows.push(html);
            if let CueBody::Group(g) = &n.body {
                visit(&g.children, depth + 1, numbers, durations, rows);
            }
        }
    }
    visit(&show.cues, 0, &numbers, durations, &mut rows);
    let total = rows.len();
    let body: String = rows.join("\n");
    let title = if show.name.is_empty() { "Sin título" } else { show.name.as_str() };
    format!(
        r#"<!doctype html>
<html lang="es"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Guion técnico — {title_e}</title>
<style>
:root{{--fg:#1b1a17;--dim:#6b665c;--line:#cfc9bb;--head:#efeadc;--accent:#8a3d08}}
*{{box-sizing:border-box}}body{{margin:0;font:13px/1.4 system-ui,"Segoe UI",sans-serif;color:var(--fg);background:#fff;padding:18px 22px}}
header{{display:flex;justify-content:space-between;align-items:flex-end;gap:16px;border-bottom:3px double var(--fg);padding-bottom:8px;margin-bottom:10px}}
header .co{{font:600 12px/1.3 Georgia,serif;letter-spacing:.12em;text-transform:uppercase}}header .co small{{display:block;font-weight:400;letter-spacing:.04em;color:var(--dim);text-transform:none}}
h1{{font:600 24px/1.1 Georgia,serif;margin:2px 0 0}}.meta{{text-align:right;color:var(--dim);font-size:12px}}
.bar{{display:flex;gap:10px;align-items:center;margin:8px 0;flex-wrap:wrap}}.bar input[type=search]{{flex:1;min-width:180px;padding:6px 8px;border:1px solid var(--line);border-radius:4px;font:inherit}}
.bar button,.bar label{{font:inherit}}.bar button{{padding:6px 12px;border:1px solid var(--fg);background:#fff;border-radius:4px;cursor:pointer}}
table{{width:100%;border-collapse:collapse;table-layout:fixed}}
th{{background:var(--head);text-align:left;font-size:11px;letter-spacing:.06em;text-transform:uppercase;border:1px solid var(--line);padding:5px 6px}}
td{{border:1px solid var(--line);padding:4px 6px;vertical-align:top;overflow-wrap:anywhere}}
td.n{{font-weight:700;white-space:nowrap}}td.t{{font-variant-numeric:tabular-nums;white-space:nowrap}}td.nt{{color:var(--dim)}}
tr.group td{{background:#f6f3ea;font-weight:600}}tr.off td{{color:var(--dim);text-decoration:line-through}}
tr{{break-inside:avoid;page-break-inside:avoid}}thead{{display:table-header-group}}
col.c1{{width:5%}}col.c2{{width:19%}}col.c3{{width:16%}}col.c4{{width:9%}}col.c5{{width:6%}}col.c6{{width:7%}}col.c7{{width:8%}}col.c8{{width:15%}}col.c9{{width:15%}}
ul.mem{{margin:0;padding-left:14px}}ul.mem b{{font-variant-numeric:tabular-nums}}
footer{{margin-top:10px;color:var(--dim);font-size:11px}}
body.bw{{filter:grayscale(1)}}
@page{{size:A4 landscape;margin:12mm 10mm}}
@media print{{body{{padding:0;font-size:10.5px;color:#000;background:#fff;filter:grayscale(1)}}.bar{{display:none}}th{{background:#e6e6e6!important;-webkit-print-color-adjust:exact;print-color-adjust:exact}}tr.group td{{background:#f0f0f0!important}}header{{border-bottom-color:#000}}
header .co,.meta,td.nt,footer{{color:#000}}a{{color:#000;text-decoration:none}}}}
</style></head><body>
<header><div><div class="co">{company}<small>{venue}</small></div><h1>Guion técnico — {title_e}</h1></div>
<div class="meta">{total} cues<br>Generado: {generated_e}</div></header>
<div class="bar"><input type="search" id="q" placeholder="Filtrar por número, nombre, archivo, tipo o nota…" aria-label="Filtrar"><label><input type="checkbox" id="bw"> Blanco y negro</label><button onclick="window.print()">Imprimir</button></div>
<table><colgroup><col class="c1"><col class="c2"><col class="c3"><col class="c4"><col class="c5"><col class="c6"><col class="c7"><col class="c8"><col class="c9"></colgroup>
<thead><tr><th>Cue</th><th>Nombre</th><th>Destino / Audio</th><th>Tipo</th><th>Pre-Wait</th><th>Duración</th><th>Sig.</th><th>Memorias</th><th>Notas de escena</th></tr></thead>
<tbody id="rows">
{body}
</tbody></table>
<footer>{company} · {venue} · TeatroCue</footer>
<script>
const q=document.getElementById('q'),rows=[...document.querySelectorAll('#rows tr')];
q.addEventListener('input',()=>{{const t=q.value.trim().toLowerCase();rows.forEach(r=>r.hidden=t&&!r.dataset.s.includes(t));}});
document.getElementById('bw').addEventListener('change',e=>document.body.classList.toggle('bw',e.target.checked));
</script>
</body></html>
"#,
        title_e = esc(title),
        generated_e = esc(generated),
        company = COMPANY,
        venue = VENUE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue_dto::{AudioTimeAndLoopsData, CueBasicsData, CueId, MediaTarget};
    use crate::show_format::{AudioBody, GroupBody, SHOW_FORMAT};

    fn audio(id: u64, number: &str, name: &str, file: &str) -> CueNodeDto {
        let mut basics = CueBasicsData::default();
        basics.unique_id = CueId(id);
        basics.number = number.into();
        basics.name = name.into();
        basics.file_target = Some(MediaTarget { absolute_path: file.into(), ..Default::default() });
        CueNodeDto { basics, triggers: Default::default(), dead: false, body: CueBody::Audio(AudioBody { time_loops: AudioTimeAndLoopsData::default(), levels: None, eq: Default::default() }) }
    }

    fn sample() -> ShowFile {
        let mut a = audio(1, "1", "Giselle <acto 1> & \"coda\"", "C:\\audio\\giselle.wav");
        a.basics.notes = "Entra la solista\nluz azul".into();
        a.basics.pre_wait = 2.5;
        a.basics.continue_mode = ContinueMode::AutoContinue;
        if let CueBody::Audio(b) = &mut a.body {
            b.time_loops.memories = vec![MemoryMark { secs: 12.345, name: "1er levantada".into(), note: "<b>ojo</b>".into() }];
        }
        let mut child = audio(3, "2.1", "Variación", "/x/var.wav");
        child.basics.armed = false;
        let mut group = audio(2, "2", "Pasada", "");
        group.body = CueBody::Group(GroupBody { mode: GroupModeDto::Timeline, children: vec![child] });
        group.basics.file_target = None;
        ShowFile { format: SHOW_FORMAT.into(), name: "Obra <1>".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![a, group] }
    }

    #[test]
    fn the_sheet_lists_every_cue_with_escaped_text_and_print_styles() {
        let durations = HashMap::from([(1u64, 125.0f64)]);
        let html = render_html(&sample(), &durations, "20/10/2026");
        for want in ["Ballet Oficial de Córdoba", "Teatro del Libertador San Martín", "giselle.wav", "02:05", "00:02.500", "Auto-continue", "00:12.345", "1er levantada", "Entra la solista<br>luz azul", "@media print", "@page", "3 cues", "Grupo · línea de tiempo", "var.wav"] {
            assert!(html.contains(want), "falta «{want}»");
        }
        // Todo el texto de usuario va escapado: ni etiquetas ni comillas sueltas.
        assert!(html.contains("Giselle &lt;acto 1&gt; &amp; &quot;coda&quot;"));
        assert!(!html.contains("<acto 1>") && !html.contains("<b>ojo</b>") && html.contains("&lt;b&gt;ojo&lt;/b&gt;"));
        // Autocontenido: ningún recurso externo.
        assert!(!html.contains("http://") && !html.contains("https://") && !html.contains("src=\""));
        // Una cue desarmada se marca y la hija queda con sangría.
        assert!(html.contains("class=\"cue off\"") && html.contains("padding-left:26px"));
        assert_eq!(html.matches("<tr class=").count(), 3);
    }

    #[test]
    fn times_are_formatted_for_the_sheet() {
        assert_eq!(fmt_time(0.0), "");
        assert_eq!(fmt_time(65.0), "01:05");
        assert_eq!(fmt_time(4.02), "00:04.020");
        assert_eq!(fmt_time(3725.0), "1:02:05");
        assert_eq!(fmt_time(f64::NAN), "");
    }
}
