use std::thread;
use std::sync::mpsc::{self, TryRecvError, Sender};
use std::time::{Duration};
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use chrono::Local;
use crate::geom::{Point, Rectangle};
use crate::input::{DeviceEvent, FingerStatus};
use crate::view::{View, Event, Hub, Bus};
use crate::framebuffer::{Framebuffer, UpdateMode, Pixmap};
use crate::font::Fonts;
use crate::color::{BLACK};
use crate::app::Context;

const RENDER_INTERVAL: Duration = Duration::from_millis(400);

struct AlgoState {
    pos: Point,
    vel: Point,
}

impl AlgoState {
    pub fn new() -> AlgoState {
        AlgoState {
            pos: Point::new(0, 0),
            vel: Point::new(16, 16),
        }
    }
}

pub struct Draw {
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
    rng: XorShiftRng,
    pixmap: Pixmap,
    random: Pixmap,
    tx: Sender<()>,
    state: AlgoState,
}

impl Draw {
    pub fn new(rect: Rectangle, hub: &Hub, _context: &mut Context) -> Draw {
        // Here to prevent errors
        let children = Vec::new();
        // let dpi = CURRENT_DEVICE.dpi;
        let mut random = Pixmap::new(rect.width(), rect.height());
        let mut rng = XorShiftRng::seed_from_u64(Local::now().timestamp_millis() as u64);
        rng.fill(random.data_mut());
        hub.send(Event::Render(rect, UpdateMode::Full)).unwrap();

        let (tx, rx) = mpsc::channel();
        
        let hub2 = hub.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(RENDER_INTERVAL);
                match rx.try_recv() {
                    Ok(_) | Err(TryRecvError::Disconnected) => {
                        break;
                    },
                    Err(TryRecvError::Empty) => hub2.send(Event::RenderTick).unwrap(),
                }
            }
        });

        Draw {
            rect,
            children,
            rng,
            pixmap: Pixmap::new(rect.width(), rect.height()),
            random,
            tx,
            state: AlgoState::new(),
        }
    }
}

#[inline]
fn draw_pix(pixmap: &mut Pixmap, state: &mut AlgoState, _rng: &mut XorShiftRng, fb_rect: &Rectangle, hub: &Hub) {
    let rect = Rectangle::from_segment(state.pos, state.pos, 16, 16);

    // pixmap.set_pixel(position.x as u32, position.y as u32, BLACK);
    pixmap.draw_disk(state.pos, 16, BLACK);

    if let Some(render_rect) = rect.intersection(fb_rect) {
        hub.send(Event::RenderNoWaitRegion(render_rect, UpdateMode::FastMono)).unwrap();
    }

    let nx = state.pos.x + state.vel.x;
    let ny = state.pos.y + state.vel.y;

    let x = if nx > fb_rect.width() as i32 || nx < 0 {
        state.vel = pt!(-state.vel.x, state.vel.y);
        state.pos.x
    } else {
        nx
    };
    let y = if ny > fb_rect.height() as i32 || ny < 0 {
        state.vel = pt!(state.vel.x, -state.vel.y);
        state.pos.y
    } else {
        ny
    };

    state.pos = pt!(x, y);
}

impl View for Draw {
    fn handle_event(&mut self, evt: &Event, hub: &Hub, _bus: &mut Bus, _context: &mut Context) -> bool {
        match *evt {
            Event::Device(DeviceEvent::Finger { status: FingerStatus::Down, id: _, position: _, time: _ }) => {
                draw_pix(&mut self.pixmap, &mut self.state, &mut self.rng, &self.rect, hub);
                let _ = self.tx.send(());
                true
            },
            Event::Device(DeviceEvent::Finger { status: FingerStatus::Up, id: _, position: _, time: _ }) => {
                hub.send(Event::Back).unwrap();
                true
            },
            Event::RenderTick => {
                draw_pix(&mut self.pixmap, &mut self.state, &mut self.rng, &self.rect, hub);
                true
            },
            _ => false,
        }
    }

    fn render(&self, fb: &mut dyn Framebuffer, rect: Rectangle, _fonts: &mut Fonts) {
        fb.draw_framed_pixmap_halftone(&self.pixmap, &self.random, &rect, rect.min);
    }

    fn render_rect(&self, rect: &Rectangle) -> Rectangle {
        rect.intersection(&self.rect)
            .unwrap_or(self.rect)
    }

    fn might_rotate(&self) -> bool {
        false
    }

    fn is_background(&self) -> bool {
        true
    }

    fn rect(&self) -> &Rectangle {
        &self.rect
    }

    fn rect_mut(&mut self) -> &mut Rectangle {
        &mut self.rect
    }

    fn children(&self) -> &Vec<Box<dyn View>> {
        &self.children
    }

    fn children_mut(&mut self) -> &mut Vec<Box<dyn View>> {
        &mut self.children
    }
}
