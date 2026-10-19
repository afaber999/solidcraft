//! A design that has just opened shows its parts as they are meshed (#3053).
//!
//! Opening hands the design to the window once its timeline is evaluated (or taken from its
//! file) and placed; the display meshes are made afterwards, off the UI thread. Until they all
//! are, the viewport draws the bodies already meshed ([`shown_world`]), so the first parts show
//! within moments and the window stays responsive while the rest come in. The first placement of
//! every part is meshed first: its other placements only move that mesh. The view is framed from
//! the bodies' outlines meanwhile, and framed again from the meshes when they are all made, unless
//! the camera was moved in between. Editing the design meanwhile meshes the new world instead.
//!
//! Display only: commands, picking of what is drawn and exports read the session's world as ever.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use solidcraft_engine::doc::ModelState;

use crate::SolveApp;

/// How often a growing picture is rebuilt at most (s): each rebuild uploads every shown body.
const REFRESH: f64 = 0.25;

pub(crate) struct Meshing {
    /// The world being meshed (the session's when it started).
    world: Arc<ModelState>,
    /// Per body of `world`: its display (mesh, and plans when drawn from them) is made.
    ready: Arc<Vec<AtomicBool>>,
    done: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    /// The picture so far: the bodies shown, and how many were ready when it was made.
    shown: Option<(usize, Arc<ModelState>)>,
    shown_at: f64,
    /// The camera the outline framing left (a refit at the end only replaces that one).
    framed: Option<crate::Camera>,
    /// Draw the design's Home picture when all is meshed.
    remember: bool,
    /// When it started, and whether the first bodies have been shown (`SOLIDCRAFT_PROFILE_OPEN`).
    began: Option<f64>,
    first_shown: bool,
}

impl Meshing {
    fn start(world: Arc<ModelState>, plans: bool) -> Meshing {
        let n = world.bodies.len();
        let ready: Arc<Vec<AtomicBool>> = Arc::new((0..n).map(|_| AtomicBool::new(false)).collect());
        let (done, stop) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        // The first placement of each part first: the others only move its mesh.
        let mut seen = std::collections::HashSet::new();
        let (mut first, mut rest) = (Vec::new(), Vec::new());
        for (i, b) in world.bodies.iter().enumerate() {
            if seen.insert(b.definition_name().to_string()) { first.push(i) } else { rest.push(i) }
        }
        first.extend(rest);
        let job = (world.clone(), ready.clone(), done.clone(), stop.clone());
        let spawned = std::thread::Builder::new().name("open-meshing".into()).spawn(move || {
            let (world, ready, done, stop) = job;
            solidcraft_engine::kernel::for_each_on_threads(&first, 16, |&i| {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(b) = world.bodies.get(i) {
                    let _ = b.mesh();
                    if plans {
                        let _ = b.display_plan();
                        let _ = b.plan_mesh();
                    }
                }
                if let Some(r) = ready.get(i) {
                    r.store(true, Ordering::Release);
                }
                done.fetch_add(1, Ordering::Release);
            });
        });
        if spawned.is_err() {
            // No thread: the viewport meshes as it draws, as before.
            done.store(n, Ordering::Release);
        }
        Meshing { world, ready, done, stop, shown: None, shown_at: f64::NEG_INFINITY, framed: None, remember: false, began: None, first_shown: false }
    }

    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire) >= self.world.bodies.len()
    }
}

impl Drop for Meshing {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start meshing the session's world off the UI thread (an opened design), framing the view from
/// the bodies' outlines; `remember`: then draw its Home picture.
pub(crate) fn start(app: &mut SolveApp, remember: bool) {
    let plans = app.viewport.gpu.is_some_and(|g| g.tess);
    let mut meshing = Meshing::start(app.session.world_state(), plans);
    meshing.remember = remember;
    app.open_meshing = Some(meshing);
    app.fit_view();
    if let Some(m) = app.open_meshing.as_mut() {
        m.framed = Some(app.cam);
    }
}

/// How far the meshing is: bodies done, and of how many.
pub(crate) fn progress(app: &SolveApp) -> Option<(usize, usize)> {
    app.open_meshing.as_ref().map(|m| (m.done.load(Ordering::Acquire).min(m.world.bodies.len()), m.world.bodies.len()))
}

/// A small "Loading parts n/N" note at the foot of the viewport while bodies are being meshed:
/// a part not drawn yet can't be picked yet either, and this says why.
pub(crate) fn show(app: &SolveApp, ctx: &egui::Context) {
    let (Some((done, total)), Some(view)) = (progress(app), app.viewport.rect) else { return };
    let t = crate::theme::Tokens::get();
    let at = egui::pos2(view.left() + 12.0, view.bottom() - 12.0);
    egui::Area::new(egui::Id::new("open_meshing"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(at)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).fill(t.panel).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new(format!("Loading parts {done}/{total}")).color(t.text));
                });
            });
        });
}

/// Whether bodies are still being meshed after an open.
pub(crate) fn active(app: &SolveApp) -> bool {
    app.open_meshing.is_some()
}

/// Per frame: follow the session's world, show more as it is meshed, and frame the finished
/// picture.
pub(crate) fn tick(app: &mut SolveApp, ctx: &egui::Context) {
    let Some(m) = app.open_meshing.as_ref() else { return };
    let world = app.session.world_state();
    if !Arc::ptr_eq(&world, &m.world) {
        // The design changed (an edit, an undo): mesh the new world; what is made is kept.
        let (framed, remember) = (m.framed, m.remember);
        let plans = app.viewport.gpu.is_some_and(|g| g.tess);
        let mut next = Meshing::start(world, plans);
        (next.framed, next.remember) = (framed, remember);
        app.open_meshing = Some(next);
    }
    let finished = app.open_meshing.as_ref().is_some_and(Meshing::finished);
    if finished {
        let Some(m) = app.open_meshing.take() else { return };
        if profiling() {
            let ms = m.began.map_or(0.0, |t| (ctx.input(|i| i.time) - t) * 1000.0);
            eprintln!("SolidCraft open meshing: all {} bodies shown after {ms:.0} ms", m.world.bodies.len());
        }
        // Framed from outlines and not moved since: frame the meshes.
        if m.framed == Some(app.cam) {
            app.fit_view();
        }
        if m.remember {
            crate::home::remember_session(&mut app.home, &app.session);
        }
    }
    ctx.request_repaint();
}

/// The world the viewport draws: while an opened design is still being meshed, the bodies meshed
/// so far (the picture grows a few times a second); otherwise the session's.
pub(crate) fn shown_world(app: &SolveApp) -> Arc<ModelState> {
    let Some(m) = app.open_meshing.as_ref() else { return app.session.world_state() };
    if !Arc::ptr_eq(&app.session.world_state(), &m.world) {
        return app.session.world_state();
    }
    m.shown.as_ref().map(|(_, st)| st.clone()).unwrap_or_else(|| Arc::new(ModelState::default()))
}

/// Remake the picture so far, at most every [`REFRESH`] seconds (each remake uploads it again).
pub(crate) fn refresh(app: &mut SolveApp, now: f64) {
    let Some(m) = app.open_meshing.as_mut() else { return };
    let done = m.done.load(Ordering::Acquire);
    if m.shown.as_ref().is_some_and(|(n, _)| *n == done) || now - m.shown_at < REFRESH {
        return;
    }
    let mut st = (*m.world).clone();
    let mut i = 0;
    st.bodies.retain(|_| {
        let keep = m.ready.get(i).is_some_and(|r| r.load(Ordering::Acquire));
        i += 1;
        keep
    });
    let began = *m.began.get_or_insert(now);
    if !m.first_shown && !st.bodies.is_empty() {
        m.first_shown = true;
        if profiling() {
            eprintln!("SolidCraft open meshing: first {} bodies shown after {:.0} ms", st.bodies.len(), (now - began) * 1000.0);
        }
    }
    m.shown = Some((done, Arc::new(st)));
    m.shown_at = now;
}

fn profiling() -> bool {
    solidcraft_engine::geom::env::var_os("PROFILE_OPEN").is_some()
}
