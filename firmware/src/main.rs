#![no_std]
#![no_main]

mod buttons;
mod display;
mod game;
mod state;
mod state_cell;
mod sweet_spot;
mod ws2812_impl;

extern crate alloc;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Delay;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    interrupt::software::SoftwareInterruptControl,
    spi::{
        Mode,
        master::{Config, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use loadcell::hx711::HX711;
use log::info;
use state_cell::StateCell;

use buttons::{Buttons, button_task};
use display::display_task;
use game::logic_task;
use state::AppState;
use sweet_spot::loadcell_task;
use ws2812_impl::{WS2812_TICKS, Ws2812Pin, Ws2812Timer};
use ws2812_timer_delay as ws2812;

/// Size of heap for dynamically-allocated memory
const HEAP_MEMORY_SIZE: usize = 72 * 1024;

static STATE: StateCell<CriticalSectionRawMutex, AppState, 1> = StateCell::new(AppState::Start);

esp_bootloader_esp_idf::esp_app_desc!();

/// Main task
#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    heap_allocator!(size: HEAP_MEMORY_SIZE);

    let timg1 = TimerGroup::new(p.TIMG1);
    let sw_int = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg1.timer0, sw_int.software_interrupt0);

    // TIMG0 drives the WS2812 bit-bang timer (timer0 @ ~3 MHz). Its watchdog
    // is not used, so disable it to prevent spurious resets while we bang on
    // timer0's registers directly.
    let mut timg0 = TimerGroup::new(p.TIMG0);
    timg0.wdt.disable();

    info!("started");

    // Display pins (ST7735s on a sensible default ESP32-C3 pinout):
    //   SCK  = GPIO8   MOSI = GPIO10
    //   CS   = GPIO21  (software CS via ExclusiveDevice)
    //   DC   = GPIO20  RST  = GPIO1
    // GPIO9 must NOT be used for RST: it is the boot-mode strapping pin and
    // being low at reset forces the chip into ROM download mode.
    let cs = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    let dc = Output::new(p.GPIO20, Level::Low, OutputConfig::default());
    let rst = Output::new(p.GPIO9, Level::High, OutputConfig::default());

    // SPI2 is the only general-purpose SPI master on the ESP32-C3.
    // 10 MHz, Mode 0, MSB-first (default) - safe for ST7735s write.
    let spi = Spi::new(
        p.SPI2,
        Config::default()
            .with_frequency(Rate::from_mhz(10))
            .with_mode(Mode::_0),
    )
    .expect("SPI2 config")
    .with_sck(p.GPIO8)
    .with_mosi(p.GPIO10);

    let state = &STATE;

    spawner.spawn(display_task(spi, cs, dc, rst, state).expect("spawn display_task"));

    // WS2812 strip: 4 LEDs on GPIO18, paced by TIMG0 timer0 ticking at
    // ~3 MHz (40 MHz APB / 2 / 13 ticks).
    let ws_timer = Ws2812Timer::new(WS2812_TICKS);
    let ws_pin = Ws2812Pin(Output::new(p.GPIO18, Level::Low, OutputConfig::default()));
    let leds = ws2812::Ws2812::new(ws_timer, ws_pin);

    // Four reaction buttons, each mapped to one LED. Pulled up so that an
    // external button to ground reads as `Level::Low` when pressed.
    let buttons = Buttons {
        btn0: Input::new(p.GPIO4, InputConfig::default().with_pull(Pull::Up)),
        btn1: Input::new(p.GPIO5, InputConfig::default().with_pull(Pull::Up)),
        btn2: Input::new(p.GPIO6, InputConfig::default().with_pull(Pull::Up)),
        btn3: Input::new(p.GPIO7, InputConfig::default().with_pull(Pull::Up)),
    };

    // HX711 load cell: SCK = GPIO2, DT = GPIO3. Used for the sweet-spot
    // hold-to-attack mechanic.
    let loadcell = HX711::new(
        Output::new(p.GPIO2, Level::Low, OutputConfig::default()),
        Input::new(p.GPIO3, InputConfig::default()),
        Delay,
    );

    spawner.spawn(button_task(state, leds, buttons).expect("spawn button_task"));
    spawner.spawn(loadcell_task(state, loadcell).expect("spawn loadcell_task"));
    spawner.spawn(logic_task(state).expect("spawn logic_task"));
}
