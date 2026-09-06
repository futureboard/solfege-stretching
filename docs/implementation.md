# Implementation: สิ่งที่สร้างแล้วและวิธีใช้

สถานะ **Implemented (M0–M5)** · 2026-09-06 · [สารบัญ](README.md) · [Summary](summary.md) · [DSP](dsp.md) · [System Design](system-design.md) · [Validation](validation.md)

เอกสารนี้บอกว่าอะไร **มีจริงในโค้ดแล้ว** อะไรวัดได้เท่าไร และอะไร **ยังไม่มี** ตัวเลขทั้งหมดในหน้านี้เป็นผลรันจริงบนเครื่องพัฒนา ไม่ใช่เป้าหมาย แต่ก็ยังไม่ใช่ผลบน reference machine ที่ตกลงกันไว้ (ยังไม่ได้เลือก)

## 1. รันได้ทันที

```bash
cargo run --bin solfege -- selftest              # correctness gates
cargo run --bin solfege -- fixtures --out ./fx   # สร้าง corpus สังเคราะห์
cargo run --bin solfege -- analyze  --in ./fx/mixed.wav
cargo run --bin solfege -- render   --in ./fx/mixed.wav --out ./out.wav \
                                    --alpha 1.5 --semitones -3 --mode polyphonic
cargo run --bin solfege -- bench    --block 256
cargo run --bin solfege -- stability --semitones 3   # ความนิ่งของโทนที่รู้ค่า
cargo run --bin solfege -- quality   --semitones 3   # bass/vocal/transient/bass+drums/tail/image
cargo run --bin solfege -- protect                   # Attack Protect ทำให้ระดับตกไหม
cargo run --features playback --bin solfege -- play --in ./fx/mixed.wav --mode polyphonic --sweep 0.8
cargo test                                       # 45 tests
cargo run --release --features demo --bin solfege-demo   # GUI
```

## 2. โครงสร้างจริง

```
src/
  lib.rs          public API + stretch_constant() ทางลัด
  audio.rs        SourceFrame/OutputFrame newtypes, AudioBuffer, AudioView(Mut),
                  SourceIdentity + FNV-1a 128-bit content hash
  mapping.rs      TimeMap (piecewise-linear, f64), forward/inverse, BeatGrid
  document.rs     EditDocument, NoteEdit, DetectedNote, FormantPolicy, serde
  analysis.rs     spectral-flux onsets + time-domain refinement, YIN F0 +
                  confidence + voicing, note segmentation, content classifier
  plan.rs         plan compiler: Auto routing, protections, conflict checks,
                  render_key, group validation
  render.rs       offline render, random-block render, exactness reconciliation
  stream.rs       real-time playback: SPSC ring, prefetch worker, pull adapter,
                  live plan swap with crossfade, underrun policy, RT metrics
  runtime.rs      InputRing / OutputAccum (preallocated, compaction, norm plane)
  dsp/            window, resample (SincBank), corr (WSOLA search), stft (realfft)
  engines/
    core.rs       EngineCore + Stepper: the shared state machine
    lowband.rs    long-window path below the crossover: F0 tracking, harmonic
                  phase locking anchored on F0, Mid-referenced stereo
    bypass.rs     identity copy
    tape.rs       band-limited resampling along W^-1
    wsola.rs      WSOLA + anchor-aware search narrowing
    percussive.rs shared slicing + sustain-only tail loop
    pv.rs         phase vocoder + peak locking + transient reset + formants
                  + HPSS masks (hybrid mode)
    texture.rs    seeded granular FX
    pitch.rs      PitchStage: stretch by alpha*p แล้ว resample ที่ p
  wav.rs          RIFF reader (PCM 8/16/24/32, float 32/64) / writer
  fixtures.rs     corpus สังเคราะห์
  metrics.rs      RMS diff, dominant_hz, onset error, channel correlation, level
  bin/solfege.rs      CLI
  bin/solfege_demo.rs egui demo
tests/
  mapping.rs      14 tests — time-map contract
  contract.rs     31 tests — process contract, exactness, quality gates
```

**ไม่มี dependency ที่ไม่จำเป็น:** `serde`, `serde_json` (จาก workspace), `rustfft`, `realfft`
เท่านั้น; `cpal` อยู่หลัง feature `playback` และ `eframe` หลัง feature `demo` เพื่อไม่ให้ไลบรารีลาก GUI หรือ audio driver ติดไปด้วย ชั้น real-time ใน `stream.rs` จึงไม่รู้จัก audio API ใดเลย

## 3. Engine ที่ implement แล้ว

| Mode | สิ่งที่ทำจริง | ratio ที่ประกาศ | formant |
|---|---|---|---|
| Bypass | copy ตรง ๆ ไม่มีเลขคณิตกับ sample | 1.0 เท่านั้น | – |
| Tape | `y[t] = interp(x, W⁻¹(t))` sinc 16 zero-crossings + Blackman-Harris, cutoff ตามความเร็วอ่าน | 0.05–20 | – |
| Percussive | cursor เดียวที่ย้ายด้วย crossfade เท่านั้น · anchor แข็งเฉพาะ attack จริง, รอยต่ออื่นและการเติมช่องว่างเลือกตำแหน่งด้วย waveform similarity (§3.7) | 0.25–8 | – |
| Monophonic | WSOLA, hann 50% overlap, search ±5 ms แบบ coarse-to-fine, offset เดียวทุกแชนเนล, search แคบลงใกล้ anchor | 0.25–4 | – |
| Polyphonic | phase vocoder hop N/4, adaptive window, identity peak locking, fractional analysis position, transient detect + phase reset, shared-rotation stereo coherence, **low-frequency specialised path** (§3.4) | 0.1–10 | ✅ |
| Hybrid | PV + HPSS soft mask (median เวลา/ความถี่) — harmonic ใช้ propagated phase, percussive ใช้ analysed phase · **window ยาวเป็น 2 เท่าของ Polyphonic** (ดู §4.1) | 0.1–10 | ✅ |
| Texture | granular 80 ms, jitter ±20 ms, PRNG มี seed | 0.05–100 | – |
| Auto | routing policy → concrete mode + เหตุผลเป็นข้อความ | ตาม mode ที่เลือก | – |

**Transposition** เป็น `PitchStage` แยกครอบ engine ใด ๆ ตาม `u(t)=∫p dq` ใน [dsp.md §3](dsp.md); เมื่อ `p = 1` stage นี้ **ไม่ถูกใส่เข้ามาเลย** ซึ่งเป็นเหตุผลที่ Bypass ยัง sample-exact ได้


## 3.1 Real-time playback (M5)

engine ทำงาน **ใน audio callback จริง** ไม่ใช่เล่น buffer ที่ render เสร็จแล้ว

```text
control thread          worker thread                 audio callback
-------------- set_plan --> compile, prepare, seek
                            warm the input ring
                            hand over a Voice ------>  swap in, crossfade
                       <-- retire the old Voice ----   (ไม่ drop ใน callback)
                            keep topping up  ------->  process() ใต้ work budget
                            the input ring
```

| องค์ประกอบ | สิ่งที่ทำ |
|---|---|
| `SpscRing` | ring ส่ง sample ข้าม thread แบบ lock-free; slot เป็น `AtomicU32` เก็บ bits ของ `f32` จึงเป็น **safe Rust ล้วน ไม่มี `unsafe`** แลกกับการเสีย vectorization ของ memcpy |
| prefetch worker | อ่าน source ป้อน ring ล่วงหน้า 0.5 s; ทำ `build_engine`/`prepare`/`reset` ทั้งหมด (allocate ได้) |
| `Voice` | engine หนึ่งตัว + ring ที่เป็นของมัน; เปลี่ยน plan = สร้าง voice ใหม่ ไม่ reconfigure ตัวเดิม |
| pull adapter | callback เรียก `process()` ซ้ำจนพอ **ใต้ work budget** (ค่าเริ่ม 32 calls); input ที่ engine ยังไม่ consume ถูกเก็บใน `pending` ไม่ทิ้ง |
| plan swap | worker warm voice ใหม่ที่ **source position เดิม** แล้ว callback crossfade แบบ **equal-gain** (ไม่ใช่ equal-power เพราะสอง signal correlated กัน ตาม system-design §11) |
| retire | voice เก่าถูกส่งกลับ worker เพื่อ drop; **ไม่มีการคืน memory ใน callback** |
| pre-roll | worker render output ของ voice ใหม่ล่วงหน้าจนคลุม crossfade ทั้งช่วง callback จึงไม่ต้องรัน engine สองตัวพร้อมกัน และ swap ไม่มีวันเงียบ (§3.6) |
| underrun | fade สั้นไป silence + นับ counter; **ไม่** สลับเป็น dry signal และ **ไม่** เปลี่ยนอัลกอริทึม |
| metrics | callbacks, underruns, starved frames, swaps, errors, **output peak + clipped frames**, histogram ของเวลา callback (log buckets) |
| output trim | gain ที่ผู้ใช้ตั้งเอง (dB) ramp ต่อ block กันเสียงซิป · **ไม่มี auto-normalize/limit** ตาม system-design §12 · วัด peak ก่อน trim เพื่อไม่ปิดบังว่า render ร้อน |

**เรื่อง lock:** เส้นทาง sample ไม่มี lock เลย ส่วนการส่งมอบ/คืน `Voice` ใช้ `Mutex::try_lock` ซึ่ง **ไม่รอ** — ถ้าชนกับ worker (ไม่กี่ instruction ที่ย้าย `Box`) callback ใช้ voice เดิมต่อและลองใหม่ block ถัดไป เขียนไว้ตรง ๆ เพราะเอกสารบอกว่า "ห้าม lock" และนี่คือข้อยกเว้นที่ตั้งใจและอธิบายได้

**ที่ยังไม่ทำในชั้นนี้:** disk-backed reader (ตอนนี้ source อยู่ใน RAM ทั้งก้อน — worker ถูกออกแบบให้เปลี่ยนเป็น reader จริงได้ที่จุดเดิม), loop, และ live input ซึ่ง `alpha>1` จะสะสม backlog ไปเรื่อย ๆ ตามที่ system-design §8 เตือนไว้

## 3.2 การปรับ DSP สำหรับไฟล์ master (รอบล่าสุด)

รอบนี้แก้เฉพาะภายใน `engines/pv.rs` กับ `plan.rs` **ไม่แตะ architecture** — contract, engine chain, pitch stage และ runtime เหมือนเดิมทั้งหมด

### สิ่งที่เพิ่ม/แก้

| หัวข้อ | เดิม | ตอนนี้ |
|---|---|---|
| **Fractional analysis position** | อ่านหน้าต่างที่ `floor(s)` แต่วัด phase advance เทียบ `s - s_prev` ที่เป็นเศษ — **สอง hop คนละค่า** | คูณ spectrum ด้วย `exp(+j·ω_k·frac)` ซึ่งเป็น fractional shift ที่ตรงตัวสำหรับเฟรมที่ window แล้ว เสียแค่การหมุนหนึ่งครั้งต่อ bin |
| **Transient detection** | มีแต่ protections จาก onset analysis (ไม่มี analysis = ไม่มี transient handling) | สองแหล่งตัดสินร่วมกัน: protections + **spectral flux ในเฟรมเอง** (adaptive median × 3.0 **และ** สัดส่วนการโตของ spectrum > 0.22) สตรีมสดที่ไม่มี analysis จึงยังรักษา attack ได้ |
| **Phase reset** | reset ทุก bin เมื่อ protections บอก | reset ทุก bin **เหนือ 160 Hz** เมื่อ transient · ใต้ 160 Hz reset เฉพาะ bin ที่ **โตเกิน 6 dB** ดู §3.3 |
| **Adaptive FFT window** | คงที่ตาม quality profile | `plan.rs` เลือกจาก analysis: พื้นจาก `N ≥ 3.5·rate/f0_low` (percentile ที่ 10 ของโน้ตที่ confident) และเพดานจาก percussivity > 0.10 ให้กลับไปใช้ค่าสั้น |
| **Peak/identity locking** | identity locking | เหมือนเดิม — **ลอง scaled locking (β = 2/(1+α)) แล้ววัดว่าแย่กว่ามาก** |
| **Stereo phase coherence** | มีอยู่แล้ว (shared `omega_hat`, หมุนทุกแชนเนลด้วยมุมเดียว) | เท่าเดิม แต่ตอนนี้ **มี test บังคับ** ทั้ง identical / inverted / mic delay |
| **COLA** | เชื่อว่าถูก | **มี test พิสูจน์**: sqrt-Hann² ที่ hop N/4 รวมได้ 2.0 คงที่ (ripple ≈ −145 dB ซึ่งคือขีดของ `f32`) และมี end-to-end test ว่าโทนนิ่งต้องไม่หายใจ |

### สองอย่างที่ลองแล้วไม่ดีขึ้น จึงไม่เอา

1. **Scaled phase locking** `β = 2/(1+α)` ที่คูณกับ offset ของ follower — vowel แกว่ง **13.09 cents rms** เทียบกับ **0.19** ของ identity เหตุผลคือ offset นั้นอธิบาย *รูปร่าง* ของ partial ข้าม bin การย่อมันคือการบิดรูป partial ไม่ใช่การผ่อนอะไร
2. **Reset เฉพาะ bin ที่โต** (ตั้งใจให้ reverb tail กับคอร์ดที่ค้างรอดผ่าน drum hit) — bin ที่ไม่ reset ยังถือ phase เดิมอยู่ พลังงานจึงลงผิดตำแหน่งในเฟรม **pre-echo −41.5 dB เทียบกับ −102.9 dB ของ full reset** ราคาที่ยอมจ่ายคือ phase กระโดดหนึ่งเฟรมในเสียงที่ค้างอยู่ ซึ่งเป็นเหตุผลว่าทำไม detector ต้องเข้มงวด

### ผลวัด (`solfege quality`, +3 semitones, alpha 1.0, polyphonic)

| | ก่อน | หลัง |
|---|---|---|
| bass 55 Hz pitch wobble | 0.37 | **0.32** cents rms |
| bass 82.5 Hz | 0.22 | **0.17** cents rms |
| **vowel /a/ 130 Hz** | 0.19 | **0.02** cents rms |
| vowel level wobble | 0.11 | 0.11 dB rms |
| drum attack rise | 1.19 → 2.91 | 1.19 → **2.72** ms |
| drum pre-echo | −120 | −102.9 dB |
| onset error | 4.50 | 4.50 ms |
| reverb tail roughness | 0.06 → 0.09 | 0.06 → **0.08** dB rms |
| stereo identical / inverted | true / −1.000 | true / −1.000 |

hybrid ได้ประโยชน์ชัดกว่า: reverb tail roughness **0.34 → 0.10** dB rms และ drum rise 3.78 → 3.48 ms

### ผลกับไฟล์ master ของผู้ใช้ (13 s, 48 kHz stereo)

| | ก่อน | หลัง |
|---|---|---|
| window ที่เลือก | 2048 (คงที่) | **4096 (85.3 ms) เลือกจาก analysis** |
| peak หลัง +3 semitones | 1.2180 (**+1.71 dBFS**) | 1.0441 (**+0.37 dBFS**) |
| clipped frames (8 วินาที) | 45 | **8** |
| underruns | 0 | 0 |

window ที่ยาวขึ้นแยก partial ของเบสได้ดีขึ้น phase จึงไม่ไปกองกันจนดัน peak — **ลด overshoot ลง 1.34 dB โดยไม่ได้ทำอะไรกับ gain เลย**

### ที่ยังไม่ดีและพูดตรง ๆ

- **alpha 1.5 ยัง smear attack อยู่มาก**: rise 1.19 → 4.59 ms, onset error 14.4 ms, pre-echo −52.3 dB ยืดมาก ๆ ด้วย PV ความละเอียดเดียวเป็นข้อจำกัดเชิงโครงสร้าง ไม่ใช่ค่าที่ปรับได้ — ทางที่เอกสารวางไว้คือ Percussive/Hybrid สำหรับเนื้อเสียงแบบนั้น หรือ multi-resolution ที่ [dsp.md §2](dsp.md) ใส่ไว้ใน backlog
- **mic delay หดตาม pitch**: 24 → 20 frames ที่ +3 semitones (24 → 24 เมื่อยืดอย่างเดียว) เพราะ PV รักษา phase difference ต่อ bin ซึ่งเท่ากับ delay คงที่ *ก่อน* resample แล้ว resample ด้วย `p` ก็หารด้วย `p` — เป็นสมบัติของการทำ pitch shift ด้วย stretch-then-resample ทุกตัว ไม่ใช่การตัดสินใจแยกแชนเนล (ยืนยันด้วย identical/inverted ที่ยังเป๊ะ) บันทึกไว้เป็น property ไม่ใช่ bug และไม่ได้ "แก้" เพราะยังไม่ชัดว่าแก้แล้วถูกกว่า
- **ยังไม่มีการฟัง** ตัวเลขข้างบนคือ correctness กับ cost ทั้งหมด

## 3.3 เบสแตกเป็นบางช่วง — ปัญหาที่ fixture เดี่ยวจับไม่ได้

รายงานว่า "เบสในเพลงแตกบางช่วง" fixture ที่มีอยู่ทั้งหมดวัดเบสได้ 0.23–0.76 cents ซึ่งดีมาก จึงเพิ่ม fixture ที่ตรงกับเคสจริง: **โน้ตเบสค้างยาว โดยมี drum hit ทับ** (`fixtures::bass_under_hits`) พร้อม metric ที่ lowpass 200 Hz แล้ววัดความขรุขระของซองเสียง (`metrics::bass_roughness_db`)

ผลแรกชี้เป้าทันที:

| fixture | pitch wobble |
|---|---|
| เบส 82.5 Hz เดี่ยว | 0.23 cents rms |
| **เบส 62 Hz มีกลองทับ** | **27.52 cents rms** |

**สาเหตุคือ transient phase reset ที่เพิ่งใส่ไปในรอบก่อน** — kick ทำให้ spectral flux พุ่ง detector จึงสั่ง reset ทุก bin รวมทั้ง bin ของเบสที่ *กำลังดังค้างอยู่* phase ของโน้ตเบสจึงถูก restart ทุกจังหวะที่มีกลอง = เบสแตกเป็นช่วง ๆ ตรงที่มีกลองพอดี ตรงกับคำว่า "บางช่วง"

### กฎที่ลงตัว (สองเงื่อนไข ไม่ใช่เงื่อนไขเดียว)

ลองสองแบบสุดขั้วก่อน แล้วทั้งคู่แลกอย่างหนึ่งไปได้อีกอย่าง:

| กฎ | เบสมีกลองทับ | drum attack rise | drum pre-echo |
|---|---|---|---|
| reset ทุก bin (รอบก่อน) | 27.52 cents | 2.37 ms | −84.8 dB |
| ไม่ reset ใต้ 160 Hz เลย | **2.46 cents** | 4.75 ms ✗ | −27.4 dB ✗ |
| **เหนือ 160 Hz reset หมด · ใต้ 160 Hz reset เฉพาะที่โตเกิน 6 dB** | **2.47 cents** | **2.36 ms** | **−75.8 dB** |

เหตุผลของกฎสุดท้าย: การ reset มีความหมายก็ต่อเมื่อหน้าต่างครอบหลายคาบ — ที่ 60 Hz หน้าต่าง 2048 @48 kHz ครอบแค่ 2.5 คาบ reset จึงไม่ได้ทำให้ attack คมขึ้นแต่ทำลายความต่อเนื่องของโน้ตที่ยังดังอยู่ · แต่ห้าม reset ใต้ 160 Hz แบบเหมารวมก็ไม่ได้ เพราะ **kick drum เองก็เป็นเสียงต่ำ** สิ่งที่แยกสองกรณีออกจากกันคือ **การโต**: bin ที่ดังอยู่แล้วคือโน้ตที่ยังสั่นอยู่ → ถือไว้ · bin ที่กระโดดขึ้นคือตัว attack เอง → reset

ผลข้างเคียงที่ดี: bass roughness ของ output (0.28 dB rms) **ต่ำกว่าของ source เอง** (0.32) เพราะ PV เกลี่ยซองเสียงเล็กน้อย

### ที่เหลือคือ clipping ล้วน ๆ

ไฟล์ของผู้ใช้ที่ +2 semitones ยังได้ peak **1.2361 (+1.84 dBFS)** และนับได้ **clipped 44 frames** ใน 10 วินาที · หรี่ตามที่ clip guard คำนวณ (−2.34 dB) → device peak **−0.50 dBFS, clipped 0** โดย engine peak ยังรายงาน 1.2361 เหมือนเดิม

clip guard **เปิดไว้เป็นค่าเริ่มต้น** แต่มันเรียนรู้จาก peak ที่วัดได้ จึงเริ่มทำงานหลังเจอ overshoot ครั้งแรก (ประมาณหนึ่งเฟรม UI ~33 ms) ถ้าอยากกันตั้งแต่แรกให้กด Render หนึ่งครั้งเพื่อดู peak แล้วตั้ง monitor trim เอง

## 3.4 Low-frequency specialised path

เบสที่ยืดแล้วออกมา robotic / phasing / chorus มีสาเหตุเดียวกันหมด: **หน้าต่างที่ใช้กับทั้งสเปกตรัมแยก partial ของเบสไม่ออก** ที่ 48 kHz หน้าต่าง 2048 วาง 55 Hz กับฮาร์มอนิกที่สองห่างกัน 2.3 bin การประมาณความถี่ของแต่ละตัวจึงปนกัน ทั้งคู่ลอยออกจากกันทีละเฟรม = chorus/phasing · แต่หน้าต่างที่แก้ได้ (16384 = 341 ms) ยาวเกินไปสำหรับส่วนที่เหลือของสเปกตรัม

จึงแยก **การวิเคราะห์** ออกเป็นสองชั้น ไม่ใช่แยก engine — `engines/lowband.rs` เป็น STFT ที่สองใน `PvEngine` เดิม มี accumulator ของตัวเอง แล้ว merge · architecture, contract, engine chain, pitch stage และ runtime ไม่ถูกแตะเลย

| สิ่งที่ขอ | ที่ทำ |
|---|---|
| FFT ใหญ่สำหรับ low band | 8192 (offline) / 4096 (realtime) — **ไม่ใช่ 16384** ดูตารางด้านล่างว่าทำไม |
| Fundamental / harmonic tracking | หา peak ที่ resolved แล้ว refine แบบ parabolic, ตรวจ sub-octave, และบังคับความต่อเนื่อง (ห้ามกระโดดเกิน 6% ต่อเฟรมขณะมั่นใจ) |
| Peak/harmonic phase locking ยึด F0 | ฮาร์มอนิกที่ h เดินด้วย `h · omega_F0 · Hs` **ไม่ใช่ค่าประมาณของ bin ตัวเอง** — นี่คือสิ่งที่หยุดไม่ให้ฮาร์มอนิกตีกันเอง · bin รอบ ๆ anchor ใช้ identity locking |
| Stereo coherence / Mid เป็น reference | ความถี่มาจาก **Mid** (ผลรวมสเปกตรัมของสองแชนเนล ไม่ต้อง FFT เพิ่ม) และ **ทุกแชนเนลถูกหมุนด้วยมุมเดียวกัน** |
| ห้าม process L/R แยก phase | ไม่มีการตัดสินใจใดต่อแชนเนลเลยใน low band — test บังคับ identical/inverted/mic-delay |
| รักษา engine เดิมสำหรับ mid/high | main band ไม่เปลี่ยนพฤติกรรม; crossover 150–210 Hz แบบ amplitude-complementary (สองฝั่งรวมกันได้ 1 ทุกความถี่) |

### ทำไมไม่ใช่ 16384 และทำไมไม่มี fallback WSOLA

วัดตามขนาดหน้าต่างของ low band (alpha 1.0, +2 semitones):

| lowfft | bass 55 Hz | bass 82.5 Hz | drum rise | drum pre-echo |
|---|---|---|---|---|
| ปิด | 0.76 | 0.23 | 2.36 ms | −75.8 dB |
| 4096 | 5.02 ✗ | 0.18 | 5.08 ms | −10.0 dB |
| **8192** | **0.36** | **0.05** | 9.09 ms | −11.3 dB |
| 16384 | 0.35 | 0.05 | 12.13 ms ✗ | −2.8 dB ✗ |

16384 ไม่ได้ดีกว่า 8192 ในเรื่องเบสเลย แต่ทำลาย attack มากกว่าเท่าตัว

**ส่วนที่สำคัญที่สุด:** ข้อเสนอให้ "fallback เป็น period-aware WSOLA แล้ว crossfade กลับเข้า spectral engine" — ผมทดลอง crossfade ระหว่างสองเส้นทางแล้ว **และมันคือตัวสร้าง artifact เอง** ไม่ว่าจะช้าหรือเร็ว:

| gate | bass under drums | drum rise |
|---|---|---|
| ไม่มี gate | 2.47 cents | 9.09 ms |
| fade 150 ms | **90.98 cents** ✗ | 2.41 ms |
| trapezoid 5 ms | **79.25 cents** ✗ | 2.36 ms |

เหตุผลตรงไปตรงมา: low band กับ main band เป็น **phase evolution คนละเส้น** ของสัญญาณเดียวกัน การ crossfade ระหว่างสองอันนี้คือ **time-varying comb filter** — เร็วแค่ไหนก็ยังเป็นการกระโดดของ phase ในเบส

ดังนั้นแทนที่จะสลับกลางทาง **ตัดสินครั้งเดียวตอน compile plan จากเนื้อเสียง**: เปิด low path เมื่อมีเบสอยู่จริง (มีโน้ตต่ำกว่า 210 Hz) และ **ไม่มี** transient ในย่านต่ำมากพอที่จะโดนทำลาย (`percussivity < 0.10`) ตัดสินครั้งเดียวจึงไม่มีรอยต่อให้เกิด artifact · โค้ด gate ยังอยู่หลัง `--lowgate` เผื่อวันหนึ่งทำให้สองแบนด์ phase-coherent กันได้จริง แต่**ปิดเป็นค่าเริ่มต้นเพราะวัดแล้วแย่กว่าไม่มี**

### ผลรวม (alpha 1.0, +2 semitones, polyphonic)

| | ก่อนรอบนี้ | หลัง |
|---|---|---|
| bass 55 Hz pitch wobble | 0.76 | **0.36** cents rms |
| bass 82.5 Hz | 0.23 | **0.02** cents rms |
| vowel /a/ 130 Hz | 0.03 | **0.01** cents rms |
| bass under drums roughness | 0.28 | **0.14** dB rms |
| drum attack rise | 2.36 | 2.36 ms (ไม่เสีย) |
| drum pre-echo | −75.8 | −75.8 dB (ไม่เสีย) |
| reverb tail roughness | 0.07 | 0.07 dB rms |
| identical / inverted stereo | true / −1.000 | true / −1.000 |

hybrid ได้ตัวเลขเท่ากับ polyphonic ทุกแถวของ low band หลังแก้บั๊กที่สำเนา low-passed ของ main band ข้าม HPSS blend ไป (ทำให้ `full − main_low` ลบผิดตัว)

### ผลกับไฟล์ master ของผู้ใช้

| | ก่อนรอบนี้ | หลัง |
|---|---|---|
| main window | 4096 | 4096 |
| low path | – | **8192 (170.7 ms) ต่ำกว่า 150–210 Hz** |
| peak ที่ +2 semitones | 1.2361 (+1.84 dBFS) | **1.1637 (+1.32 dBFS)** |
| underruns (live 10 s) | 0 | 0 |

### กฎการเลือกหน้าต่างที่เปลี่ยนไปด้วย

เมื่อ low path ทำงาน หน้าต่างของ main band **ไม่ต้องแยกเบสอีกต่อไป** แต่ยังต้องวาด crossover ให้ตรงกับที่ low band วาด ไม่งั้นสองฝั่งรวมกันไม่ได้ 1 พอดี — วัดได้ว่าหน้าต่าง 2048 ที่วาง transition กว้าง 60 Hz ลงแค่ 2 bin ดัน peak ขึ้นเป็น **+3.01 dBFS** เทียบกับ **+1.32** ของ 4096 กฎจึงเปลี่ยนจาก "ต้องแยก partial ของโน้ตต่ำสุด" เป็น "ต้องมี 5 bin คร่อม transition"

และ hybrid เลิกคูณสองเมื่อ low path ทำงาน (HPSS ถูกถามเฉพาะเหนือ crossover แล้ว) ซึ่งลดต้นทุน hybrid จาก 33.3% เหลือ **17.3%** ของ deadline

### ราคาที่จ่าย

| Engine | p99 ก่อน | p99 หลัง | % ของ deadline |
|---|---|---|---|
| polyphonic | 0.111 ms | 0.331 ms | 6.2 % |
| hybrid | 0.466 ms | 0.921 ms | 17.3 % |

profile เป็น **spiky**: p50 ตกลงเหลือ 0.007 ms เพราะ callback ส่วนใหญ่ไม่ทำอะไร แล้ว FFT ก้อนใหญ่ตกลงมาใน callback เดียว (max 2.05 ms) · ยังไม่มี callback ไหนเกิน deadline แต่นี่คือจุดที่ต้อง optimize ถ้าจะรันหลาย track พร้อมกัน

**latency เพิ่ม:** low band มองไปข้างหน้าเต็มหน้าต่างของมัน (170 ms offline) `LatencyInfo::lookahead_input_frames` รายงานค่าที่ใหญ่กว่าแล้ว

## 3.5 Attack Protect: แก้อาการเสียงวูบเหมือน sidechain

รายงานว่าเปิด Attack Protect แล้วเสียงวูบเหมือน gate/sidechain · ไล่โค้ดก่อนตามที่ขอ — **ไม่มี `output *= mask` หรือ envelope ใด ๆ ในเส้นทาง PV เลย** ตัวคูณ amplitude ตัวเดียวในโปรเจกต์อยู่ใน `percussive.rs` (ดู §3.5.3)

แล้วอาการมาจากไหน — วัดด้วย `solfege protect` (render ชิ้นเดียวกันสองครั้ง protect off/on แล้วเทียบพลังงาน):

| | total dB | รอบ transient (worst) | peak dB |
|---|---|---|---|
| polyphonic / drums | −1.73 (per-block mean) | **−61.18** | −1.51 |
| hybrid / drums | −3.70 | −47.91 | −0.14 |
| polyphonic / mixed | −0.84 | −7.11 | −1.90 |

**ต้นเหตุคือ hard phase reset เอง** ไม่ใช่ gain ที่ hop N/4 มีสี่เฟรมซ้อนกันอยู่ เฟรมที่ phase กระโดดกลับไปเป็นค่า analysed จึงไม่ตรงกับอีกสามเฟรมที่เขียนไว้แล้วรอบ ๆ overlap-add เลยหักล้างกัน — **ระดับตกลงมาจาก interference ไม่ได้มีใครคูณอะไร**

### 3.5.1 แก้: re-anchor แทน reset

เก็บ **โครงสร้าง phase ที่วิเคราะห์ได้** ซึ่งเป็นส่วนที่ทำให้ attack คม แล้วหมุนทั้งชุดด้วย **delay + ค่าคงที่หนึ่งค่า** ที่เข้ากับ trajectory เดิมที่สุด:

- **delay** ประมาณจากผลต่าง phase ระหว่าง bin ข้างเคียง (ทนต่อการ wrap เพราะสำหรับ delay `tau` ผลต่างนี้คงที่เท่ากับ `2π·tau/N` ไม่ว่า phase สัมบูรณ์จะเป็นเท่าไร) — **delay คือ time shift แท้ ๆ จึงไม่ทำให้ waveform เพี้ยน** ต่างจากการหมุนด้วยค่าคงที่อย่างเดียวซึ่งเป็นการบิดแบบ Hilbert และวัดได้ว่าทำ attack นุ่มลง (rise 2.36 → 3.56 ms)
- **ค่าคงที่** คือส่วนที่เหลือหลังหัก delay ออก
- ทั้งสองค่าเป็น **ค่าเดียวใช้ร่วมกันทุกแชนเนลและทุก bin** — การหมุนร่วมไม่ขยับ partial ใดเทียบกับ partial อื่น และไม่ขยับ L เทียบกับ R
- **ไม่มีการ crossfade ระหว่าง phase trajectory คนละชุด** ที่ไหนเลย

### 3.5.2 ผลวัด

`solfege protect` (พลังงานในหน้าต่าง −50..+150 ms รอบแต่ละ transient, protect-on ลบ protect-off):

| | hard reset | **re-anchor** |
|---|---|---|
| polyphonic drums — total | −0.72 dB | **−0.08 dB** |
| polyphonic drums — worst hit | −2.09 dB | **−0.17 dB** |
| polyphonic mixed — worst hit | −4.09 dB | **−1.20 dB** |
| polyphonic mixed — peak | −2.26 dB | **+0.07 dB** |
| hybrid mixed — worst hit | −3.49 dB | **−1.00 dB** |

และ **ยังทำหน้าที่ของมันอยู่** — เทียบ protect off กับ on:

| protect | drum attack rise | pre-echo |
|---|---|---|
| 0 ms (ปิด) | 6.15 ms | −29.8 dB |
| **6 ms** | **3.18 ms** | **−36.3 dB** |
| 12 ms | 4.38 ms | −32.9 dB |
| 24 ms | 5.79 ms | −30.2 dB |

attack คมขึ้นเกือบเท่าตัวโดยเสียระดับไม่เกิน 1.2 dB ในหน้าต่างไหนเลย และพลังงานรวมแทบไม่ขยับ

### 3.5.3 ตัวคูณ amplitude ตัวเดียวที่มี — เอาออกแล้ว

`percussive.rs` เคย fade หาง slice ลงเป็นศูนย์เมื่อบีบเวลา (`v * tail_gain`) นั่นคือ gain envelope จริง ๆ และมันโผล่มาเมื่อ Attack Protect สร้าง cut point เท่านั้น

เปลี่ยนเป็น **กระโดดตำแหน่งอ่านแทน**: crossfade แบบ equal-power ไปข้างหน้าเท่ากับส่วนเกินพอดี (`src_len − out_len`) slice จึงจบที่ sample ก่อน attack ถัดไป **ที่ระดับเต็ม** · equal-power เพราะสองฝั่งของรอยต่อเป็นคนละช่วงของหางที่กำลังตาย ไม่ correlate กัน (ต่างจากกรณีที่ system-design §11 เตือน)

ตอนนี้ **ไม่มี `output *= mask` / `*= envelope` / amplitude gating เหลืออยู่ในโปรเจกต์**

### 3.5.4 `attack_protect_ms = 0` คือปิดจริง

เดิม 0 แค่ทำให้ protection window ว่าง แต่ **detector ภายในยังทำงาน** — ยังมี phase reset อยู่ จึงไม่ใช่ bypass

ตอนนี้ `PreparedConfig` มี `transient_protect: bool` แยกจาก `protections` ที่ว่าง: 0 ms ปิดทั้ง detector ทั้ง re-anchor ทั้ง low-band gate · ยืนยันด้วย test `zero_attack_protect_is_a_real_bypass` และเห็นได้จากตัวเลข (rise 6.15 ms = ไม่มี transient handling เลย)

ส่วนค่าเป็นมิลลิวินาทีมีความหมายจริงสามที่: WSOLA ใช้กันไม่ให้ search ตกลงในบริเวณ attack, Percussive ใช้เป็นความยาว attack ที่ห้ามเอาไป loop, และ PV ใช้เป็น **refractory period** — หลัง re-anchor แล้วอีก N มิลลิวินาทีถัดไปถือว่ายังเป็น attack เดิม ไม่ใช่ attack ใหม่ · ยาวขึ้น = re-anchor น้อยลง = นุ่มขึ้นแต่ attack ทื่อลง ซึ่งเป็น trade ที่เห็นตรง ๆ ในตารางข้างบน · ค่าเริ่มต้นใน demo เปลี่ยนเป็น **6 ms**

### 3.5.5 Test ที่เพิ่ม

- `attack_protect_does_not_duck_the_level` — สาม fixture × สอง engine: พลังงานรวมต้องไม่ตกเกิน 0.6 dB, พลังงานในหน้าต่างรอบทุก transient ไม่ตกเกิน 2 dB, peak ไม่ตกเกิน 1.5 dB
- `zero_attack_protect_is_a_real_bypass` — 0 ms ต้องปิด `transient_protect` ไม่ใช่แค่ทำให้ window ว่าง
- `attack_protect_changes_phase_not_scale` — render ซ้ำต้องได้บิตเดียวกัน, เปิด/ปิดต้องได้เสียงต่างกัน แต่ RMS รวมต้องต่างไม่เกิน 0.6 dB

## 3.6 กระตุกตอนปรับ control ระหว่างเล่นสด

รายงานว่าขยับ slider แล้วเสียงกระตุก · ไล่แล้วเป็น **สามบั๊กต่อกันเป็นลูกโซ่** ไม่ใช่เรื่องความเร็ว

### บั๊กที่ 1 — voice ใหม่ถูกส่งไปทั้งที่ engine ยังไม่ได้ warm up

worker warm แค่ **input ring** แล้วส่ง voice ให้ callback ทันที แต่ phase vocoder **ไม่ผลิตอะไรเลย** จนกว่าจะ overlap-add ครบ warm-up ซึ่งกับ low band คือ **~128 ms** · callback จึงได้ voice ที่เงียบสนิทมา แล้วโค้ด crossfade เขียนไว้ว่า

```rust
let made = if fading { a_made.min(b_made) } else { a_made };
```

`b_made = 0` → `made = 0` → เข้าทาง underrun → **fade ลงเงียบทั้ง block** ทุกครั้งที่ขยับ slider

แก้สองชั้น:
- **worker render output จริงล่วงหน้า** (`Voice::warm`) จนครบ `swap_fade + max_block` เฟรม แล้วค่อยส่งมอบ · callback จึงได้ voice ที่มีเสียงพร้อมเล่นทันที และระหว่าง crossfade ก็แค่ copy ไม่ต้องรัน engine สองตัวพร้อมกัน
- callback ไม่เอา `min()` มาตัดเสียงของ voice เดิมอีกต่อไป: blend ได้เท่าไรก็เท่านั้น ที่เหลือเล่น voice เดิมเต็มระดับ · **การ swap ต้องไม่เป็นเหตุให้เงียบ**

### บั๊กที่ 2 — seek กับ low band: ring เริ่มช้าไป

`PvEngine::reset` คำนวณจุดเริ่มของ input ring จาก lead ของ **main band** แต่ low band เริ่ม warm-up ก่อนหน้านั้นเต็มหนึ่งหน้าต่าง → low band ขาด source ตลอดกาล → ไม่ผลิต → merge ไม่ผลิต → voice เงียบ · แก้ให้ใช้ **lead และ window ของแบนด์ที่กว้างที่สุด**

### บั๊กที่ 3 — deadlock ใน pitch stage ตอน seek (ตัวจริง)

`PitchStage::reset` ตั้ง label ของ intermediate ring ไว้ที่ `output_frame*p − context` แต่เฟรมแรกที่ inner engine ผลิตจริงอยู่ที่ `round(output_frame*p)` · **การปัดเศษจึงเป็นตัวตัดสิน**: ถ้าปัดขึ้น ring จะอ้างว่าเริ่มหลังตำแหน่งที่ resampler ต้องการ → `holds()` ไม่มีวันเป็นจริง → ring เต็ม → `z.space() == 0` → **inner engine ไม่ถูกเรียกอีกเลย** → stage ผลิตศูนย์ตลอดไป

เกิดประมาณ **ครึ่งหนึ่งของทุก seek** ตามทิศทางการปัด — วัดได้จาก log ของ worker: voice ที่ preroll สำเร็จกับที่ได้ 0 สลับกันไปมา

แก้: label ring ที่ตำแหน่งจริงของเฟรมแรก และให้ readiness test ถือว่า context ที่เลยขอบออกไปคือ **zero padding ที่นิยามไว้** ไม่ใช่เหตุให้รอ — กฎเดียวกับที่ engine ใช้กับ source อยู่แล้ว

**offline ไม่มีทางเจอ** เพราะไม่เคย seek และ test เดิม `reset_to_zero_matches_a_fresh_prepare` ทดสอบแค่ตำแหน่ง 0 ซึ่งเป็นตำแหน่งเดียวที่ `0*p` ปัดลงตัวพอดี

### ผล

| | ก่อน | หลัง |
|---|---|---|
| swap ที่ถึง callback (10 s, สั่งทุก 0.3 s) | 3 | **25** |
| underruns | 0 | 0 |
| callback max ขณะ swap | 8.26 ms | **3.80 ms** |
| callback max ไม่ swap | 2.85 ms | 3.09 ms |
| voice ที่ warm สำเร็จ | ~50% | **100%** |

deadline ของ device คือ 10 ms

### Test ที่เพิ่ม

`seeking_with_a_transpose_keeps_producing` — 3 engine × 3 ค่า transpose × 5 ตำแหน่ง seek: หลัง `reset` ต้องผลิต output ได้ภายใน 200 call · **ยืนยันแล้วว่า test นี้ fail จริงกับโค้ดเดิม** (`polyphonic at +2 semitones produced nothing after a seek to 3900`) ไม่ใช่ test ที่เขียนให้ผ่านเฉย ๆ

## 3.7 Percussive: anchor transient โดยไม่ restart phase timeline

Percussive วาง attack ได้แม่นที่สุดในบรรดา engine ทั้งหมด — rise 1.17 ms จาก source 1.19 ms, onset error 0.85 ms — แต่ทำลายทุกอย่างที่ sustain อยู่ใต้มัน เพราะ **การ anchor transient กับการ restart waveform ไม่ใช่เรื่องเดียวกัน** และเวอร์ชันแรกทำอย่างที่สองขณะพยายามทำอย่างแรก

โค้ดเดิมคำนวณตำแหน่งอ่านจาก offset ภายใน slice โดยตรง (`read = src_start + j`) แปลว่า **ทุกรอยต่อ slice และทุกรอยต่อ loop คือการเริ่มใหม่** ของสัญญาณที่ไม่เคยหยุด:

| | ก่อน |
|---|---|
| bass 55 Hz | **113.02 cents rms** |
| bass 82.5 Hz | 38.34 cents |
| vowel /a/ | 3.68 cents |
| reverb tail roughness | **6.07 dB rms** (source 0.06) |

### เปลี่ยนเป็น cursor ที่ขยับด้วย crossfade เท่านั้น

ตำแหน่งอ่านกลายเป็น **cursor** ไม่ใช่ฟังก์ชันของ output offset · เดินหน้าทีละเฟรมต่อหนึ่ง output frame และย้ายได้ทางเดียวคือผ่าน crossfade:

- **ที่ attack จริง** cursor ถูกดึงไปที่ anchor เพราะนั่นคือเหตุผลที่ engine นี้มีอยู่ · การย้ายเกิดใน **บริเวณก่อน attack** และจบพอดีที่ตัว attack เอง — transient จึงยังลงตรงที่ map กำหนด ส่วนรอยต่ออยู่ตรงที่ไม่มี transient ให้เสียหาย · สองจุดนี้ไม่เกี่ยวกัน จึง crossfade แบบ **equal power**
- **ที่รอยต่อซึ่งไม่ใช่ attack** — คือรอยที่เกิดจากการซอย span ยาวเป็นท่อน ๆ ไม่มี transient อยู่ — **ไม่ anchor** แต่เลือกตำแหน่งด้วย waveform similarity · เนื้อเสียง tonal ไม่มี onset เลย ทุกรอยต่อจึงเป็นแบบนี้และ phase เดินต่อเนื่องตลอด
- **ตอนเติมช่องว่าง** cursor กระโดดถอยหลังด้วยระยะที่เลือกจาก **waveform similarity ไม่ใช่เลขคณิต** · การกระโดดคงที่จะเข้า phase ก็ต่อเมื่อบังเอิญเป็นจำนวนเต็มของคาบ ซึ่งกับโน้ตเบสแทบไม่เคยเกิด · สองฝั่งเป็น waveform เดียวกันจึง crossfade แบบ **equal gain**

**search ต้องกว้างพอ** — อย่างน้อยหนึ่งคาบของโน้ตต่ำสุดที่คาดว่าจะเจอ · ที่ 55 Hz คาบคือ 18 ms และ search ±6 ms ทำให้เบสยังเพี้ยน 26 cents เพราะตำแหน่งที่ต้องการอยู่นอกระยะเอื้อม · ขยายเป็น **±26 ms**

**ระยะกระโดดถอยหลังถูกจำกัดเท่าที่ช่องว่างต้องการ** — บนหางที่กำลังตาย การกระโดดคือ *ขั้นของระดับ*: ถอย 120 ms ในหางที่ decay 260 ms ไปโผล่ที่ดังกว่า 4 dB · กระโดดสั้นหลายครั้งดีกว่ากระโดดยาวครั้งเดียว

### ผล (alpha 1.4)

| | ก่อน | หลัง |
|---|---|---|
| bass 55 Hz | 113.02 | **0.16** cents rms |
| bass 82.5 Hz | 38.34 | **0.10** cents rms |
| vowel /a/ 130 Hz | 3.68 | **0.01** cents rms |
| reverb tail roughness | 6.07 | **1.46** dB rms |
| bass under drums | 54.50 | **38.79** cents |
| **drum attack rise** | 1.17 | **1.19 ms** (source เท่ากับ 1.19) |
| **drum pre-echo** | −120 | **−120 dB** |
| **drum onset error** | 0.85 | **0.85 ms** |
| mic delay 24 frames | 24 | 24 |

**การ anchor ไม่เสียอะไรเลย** — attack rise ตอนนี้เท่ากับ source พอดี ส่วนเบสดีขึ้น 700 เท่า

ราคา: percussive p99 0.005 → **0.594 ms** (11.1% ของ deadline) จาก correlation search ที่รอยต่อ

### ที่ยังเหลือ และเป็นข้อจำกัดเชิงโครงสร้าง

- **bass under drums ยัง 38.79 cents** — ตรงนั้นรอยต่อเป็น attack จริง จึงต้อง anchor แข็ง และ anchor แข็งคือการ restart phase ของเบสที่อยู่ใต้กลอง · จะให้ทั้งสองอย่างพร้อมกันไม่ได้: คาบของ 55 Hz คือ 873 เฟรม ขณะที่ความคลาดเคลื่อนของ attack ที่ยอมรับได้คือ ~48 เฟรม จึงไม่มีตำแหน่งไหนที่ทั้ง anchor ตรงและ phase ต่อเนื่อง · เนื้อเสียงแบบนี้ควรไปที่ Polyphonic/Hybrid ซึ่งวัดได้ 2.12 cents (Auto ส่งไปเองอยู่แล้วเพราะ percussivity ต่ำ)
- **reverb tail ยัง 1.46 dB** — การเติมช่องว่างด้วยการวนซ้ำบนหางที่ decay ย่อมเป็นขั้นของระดับเสมอ ไม่ว่าจะจัด phase ดีแค่ไหน · หางเป็นงานของ engine เชิงสเปกตรัมที่ *ยืด* แทนที่จะ *วน*

### Test

`percussive_anchors_transients_without_restarting_phase` ตรวจสองด้านพร้อมกัน: โทน sustain (ไม่มี onset จึงไม่มีอะไรถูก anchor) ต้องแกว่ง < 3 cents ที่ 55/82.5/220 Hz **และ** impulse train ทุกลูกต้องลงห่างจาก `W(s)` ไม่เกิน 3 ms · ยืนยันแล้วว่า test fail จริงเมื่อลดระยะ search (`percussive restarted the phase of a 55 Hz tone: 51.32 cents rms`)

## 4. ผลวัดจริง (ไม่ใช่เป้าหมาย)

`solfege selftest` ผ่านทุก gate ที่ 44.1 / 48 / 96 kHz:

| Gate | เกณฑ์จาก validation.md | วัดได้ |
|---|---|---|
| identity bypass | sample-exact | **bit-identical** ทุก block size (1, 17, 64, 127, 256, 1024) |
| endpoint = `M` | ทุกครั้ง | **exact** ทั้ง 6 engine × alpha 0.5/0.75/0.9/1/1.1/1.5/2 |
| inverse round-trip | ≤ 0.5 frame | **≤ 0.5 frame** (map สองช่วง, 2000 จุด) |
| block invariance | RMS diff ≤ −100 dBFS | **−inf dBFS** (บิตตรงกัน) ทั้ง fixed-17 และ random blocks |
| steady pitch | median ≤ 1 cent | **+0.01 cents** ที่ +7 semitones |
| hard anchor | ≤ 1 sample frame (scheduler) | impulse ห่างจาก anchor **0.062 ms** (3 frames @48k) |
| stereo inversion | ต้องรักษาความสัมพันธ์ | correlation **−1.0000** |
| silence | ไม่สร้างพลังงานเอง | peak **0.0** |
| typed rejection | ก่อน render | ผ่านทั้ง tape+transpose, crossed anchors, internal ratio, formant-unsupported, protection conflict |

`solfege bench --block 256` (48 kHz, alpha 1.5, quality realtime, deadline 5.333 ms):

| Engine | p50 | p99 | max | p99/deadline |
|---|---|---|---|---|
| tape | 0.058 ms | 0.121 ms | 0.316 ms | 2.3 % |
| monophonic (WSOLA) | 0.002 ms | 0.198 ms | 0.299 ms | 3.7 % |
| polyphonic | 0.007 ms | 0.533 ms | 1.473 ms | 10.0 % |
| hybrid | 0.007 ms | 0.999 ms | 1.162 ms | 18.7 % |
| percussive | 0.002 ms | 0.594 ms | 1.930 ms | 11.1 % |
| texture | 0.003 ms | 0.024 ms | 0.068 ms | 0.4 % |

ทุกตัวอยู่ใต้เป้า p99 ≤ 50% ของ deadline โดยไม่มี call ไหนเกิน deadline เลย · ตัวเลข WSOLA เป็นค่า**หลัง**เปลี่ยนเป็น coarse-to-fine search (เดิม p99 1.023 ms / max 1.420 ms ซึ่งพุ่งถึง 7.61 ms บนเพลงจริง ดู §5)

### ผลจาก audio callback จริง

`solfege play` เล่นไฟล์ 3 s ที่ alpha 1.5 ผ่าน WASAPI (block 480 frames @48 kHz จึงมี deadline 10 ms) จนจบทุกโหมด:

| Engine | p99 | max | underruns |
|---|---|---|---|
| tape | 0.51 ms | 1.19 ms | 0 |
| monophonic | 1.84 ms | 1.84 ms | 0 |
| polyphonic | 0.51 ms | 0.94 ms | 0 |
| hybrid | 1.02 ms | 2.42 ms | 0 |
| percussive | 0.13 ms | 0.52 ms | 0 |
| texture | 0.13 ms | 0.16 ms | 0 |

`play --sweep 0.8` เปลี่ยน plan กลางสตรีม 5 ครั้ง (alpha 1.5 → 0.8 → 2.0 → 1.0 → 0.6) ระหว่างเล่น: **0 underruns** และ playhead เดินต่อเนื่องไม่กระโดด เพราะ swap รักษา source position ไว้

*หมายเหตุ:* p99 ที่รายงานเป็น **ขอบบนของ bucket** ใน histogram แบบ log จึงหยาบระดับเท่าตัว และถูก clamp ไม่ให้เกิน max ที่วัดได้จริง

**ข้อจำกัดของตัวเลขชุดนี้:** วัดบนเส้นทาง offline ของเครื่องพัฒนาเครื่องเดียว ไม่ได้รันบน audio thread จริง ไม่ได้วัดหลาย track พร้อมกัน และยังไม่ได้เลือก reference machine จึงยังไม่ใช่คำสัญญาเรื่อง latency


### 4.1 ความนิ่งของเสียง (`solfege stability`)

ป้อนโทน harmonic สังเคราะห์ที่รู้ `f0` แล้ววัดสองอย่างที่ **เป็นศูนย์ก่อนเข้า engine** — เพราะ "ฟังไม่ smooth" หมายถึงคนละอาการและแก้คนละทาง:

- **pitch wobble (cents)** — fundamental แกว่งรอบค่าเฉลี่ยตัวเอง; เกิดเมื่อ window แยก partial ไม่ออก
- **level wobble (dB)** — ซองเสียงหายใจ; คือ phasiness

ที่ +3 semitones, alpha 1.0, 48 kHz:

| engine | 55 Hz | 82.5 Hz | 110 Hz | 220 Hz | 440 Hz |
|---|---|---|---|---|---|
| monophonic (WSOLA) | 1.47 | 2.34 | 0.09 | 0.08 | 0.08 |
| polyphonic | 0.37 | 0.22 | 0.23 | 0.18 | 0.18 |
| hybrid **ก่อนแก้** | **10.48** | 3.48 | 0.61 | 0.18 | 0.18 |
| hybrid **หลังแก้** | **1.14** | 0.34 | 0.28 | 0.25 | 0.24 |

(หน่วย cents rms · level wobble อยู่ที่ 0.02–0.20 dB rms ทุกโหมด ไม่ต่างกันมาก)

**ทำไม hybrid ถึงพังที่ย่านต่ำ:** HPSS ประมาณส่วน percussive ด้วย median ตามแกนความถี่ ซึ่งจะเจอ "พื้นแบบ broadband" ก็ต่อเมื่อ window แยก partial ข้างเคียงออกจากกันได้ โน้ตเบสวาง partial ห่างกันเท่ากับ `f0` — ที่ 55 Hz กับ window 2048 @48 kHz คือห่างกันแค่ **2.35 bin** median จึงไปนั่งทับ partial เอง mask เลยยุบเข้าหา 0.5 แปลว่า **ครึ่งหนึ่งของเสียงเบสถูกส่งไปทาง percussive ที่ใช้ analysed phase** แล้วสั่น

ยืนยันด้วยการไล่ขนาด window: hybrid ที่ 55 Hz ได้ 10.48 → 1.14 → 0.43 cents ที่ 2048 → 4096 → 8192 ขณะที่ polyphonic แทบไม่ขยับ (0.37 → 0.30 → 0.21) จึงชัดว่าเป็นเรื่อง **mask ไม่ใช่ phase propagation**

**แก้:** hybrid ใช้ window ยาวเป็นสองเท่าของ polyphonic (offline 4096, realtime 2048) เลือก 4096 ไม่ใช่ 8192 เพราะ 8192 = 170 ms ซึ่งเบลอ attack เกินไปสำหรับ mix ที่มีกลอง · polyphonic **ไม่** ขยับตาม เพราะข้อมูลชุดเดียวกันบอกว่าที่ 4096 มัน *แย่ลง* เล็กน้อยเหนือ 110 Hz · ราคาที่จ่าย: hybrid p99 0.278 → 0.462 ms (8.7% ของ deadline)

**ที่ยังไม่ดี:** WSOLA ที่ 55–82 Hz (1.47–2.34 cents) — search ±5 ms = 240 samples ขณะที่คาบของ 55 Hz คือ 873 samples และ overlap 15 ms ก็สั้นกว่าคาบที่ 82.5 Hz พอดี ตรงกับที่ [dsp.md §4](dsp.md) เขียนเตือนไว้เองว่าค่าเหล่านี้เป็นจุดเริ่มต้น และ "เสียงเบส period ยาวต้องทดสอบต่างหาก" ยังไม่ได้แก้

## 5. บั๊กที่การทดสอบและการฟังจับได้ (บันทึกไว้เพราะมีประโยชน์)

1. **parabolic peak interpolation เครื่องหมายกลับด้าน** — สูตรถูกคือ `p = 0.5(a−c)/(a−2b+c)` โค้ดแรกเขียนกลับด้าน ทำให้ตัววัดรายงาน pitch error +14.92 cents ทั้งที่ DSP ถูก บั๊กอยู่ใน *ตัววัด* ไม่ใช่ engine — ซึ่งเป็นเหตุผลที่ [validation.md §5](validation.md) ยืนยันว่าห้ามใช้ detector ตัวเดียวกับ engine เป็นกรรมการ (บั๊กแบบเดียวกันอยู่ใน YIN refinement ด้วย แก้พร้อมกัน)
2. **onset ถูก quantize เป็น STFT hop** — flux peak ที่เฟรม `m` บอกได้แค่ว่าพลังงานโผล่ที่ไหนสักแห่งใน window 2048 samples = คลาดได้ถึง 42 ms ทำให้ anchor test พลาดไป 39.5 ms แก้ด้วยการ refine ในโดเมนเวลา (หาจุดที่ short-term energy ขึ้นชันที่สุดใน span นั้น) → เหลือ 0.062 ms
3. **phase vocoder ไม่มีวัน `Finished` หลัง `reset`** — เจอตอนต่อ real-time เท่านั้น: `reset` ตั้ง `raw_produced = output_frame + lead` ขณะที่ `out.skip(lead)` ก็ตัด warm-up อยู่แล้ว จึงนับ lead สองรอบ ผลคือทิ้ง output จริง `lead` เฟรมแรกและจบก่อนถึง `M` ทำให้ stream underrun ทุก callback หลังจบเพลง **offline ไม่มีทางเจอเพราะไม่เคย seek** เพิ่ม test `reset_to_zero_matches_a_fresh_prepare` ที่บังคับว่า reset ไปศูนย์ต้องให้ผลตรงกับ prepare สดทุกโหมด


### จากไฟล์เพลงของผู้ใช้ (13 s, 48 kHz stereo, mastered)

รายงานเพิ่มว่า "ยังไม่ค่อย smooth" — ไล่ต่อได้อีกสองเรื่อง

7. **Auto ส่งเพลงที่แทบไม่มี transient ไปเข้า Hybrid** — ไฟล์นี้ analyse ได้ `percussivity 0.013`, `tonality 0.743` ตัว classifier เดิมเช็ค tonality ก่อน จึงตกลงมาเป็น `Mixed` → Hybrid ทั้งที่ **แทบไม่มีอะไรให้ HPSS แยกเลย** และ Hybrid คือโหมดที่เพี้ยนที่สุดในย่านต่ำ (ตาราง §4.1) แก้ให้ classifier ตัดสินจาก percussivity ก่อน: ถ้าต่ำกว่า 0.05 ให้เป็น `PolyphonicTonal` เพราะการแยก harmonic/percussive จะคุ้มก็ต่อเมื่อมี percussive ให้แยก · ผลคือไฟล์นี้ย้ายจาก hybrid ไป **polyphonic** (`ANALYZER_VERSION` เลื่อนเป็น 2 เพื่อไม่ให้ cache เก่าถูกใช้ต่อ)

8. **เสียงแตกที่ย่านต่ำคือ clipping ไม่ใช่ artifact ของ DSP** — วัดกับไฟล์จริง:

   | | peak | dBFS |
   |---|---|---|
   | source (bypass) | 0.9751 | −0.22 |
   | polyphonic +3 semitones | **1.2180** | **+1.71** |
   | polyphonic alpha 1.5 | **1.2658** | **+2.05** |
   | hybrid +3 semitones | 0.9477 | −0.47 |
   | monophonic +3 semitones | 0.9944 | −0.05 |

   phase vocoder เปลี่ยนความสัมพันธ์ของเฟส partial ที่เคยหักล้างกันจึงมาบวกกัน — **บวก peak ขึ้นเกือบ 2 dB** ไฟล์ที่ master มาที่ −0.22 dBFS จึงทะลุ full scale แน่นอน และย่านต่ำซึ่งพลังงานมากที่สุดแตกก่อน สตรีมจริง 8 วินาทีนับได้ **clipped 50 frames**

   ทางแก้ต้องแยกสองเรื่องให้ชัด:
   - **render/export ไม่แตะ** ตาม system-design §12 — แต่ `solfege render` บอกตรง ๆ ว่าเกินไปกี่ dB และต้องลดเท่าไรถึงจะพอดี
   - **monitor path มี clip guard** (เปิดไว้เป็นค่าเริ่มต้น ปิดได้) ดึง gain ของสิ่งที่ส่งออก device ลงเท่าที่ peak ที่วัดได้เรียกร้อง ลงอย่างเดียวไม่ขึ้น จึงนิ่งไม่ปั๊ม และ **meter ยังรายงาน peak จริงก่อน trim** เพื่อไม่ปิดบังว่า render ยังร้อน · การฟัง engine ผ่าน converter ที่ clip อยู่ไม่ได้บอกอะไรเกี่ยวกับ engine เลย

### จากการฟังจริงบนไฟล์เพลงยาว 5 นาที

รายงานว่า "ปรับ pitch แล้วย่านต่ำแตก และมีเสียงกระตุก" — ไล่แล้วเจอสามเรื่องแยกกัน ไม่ใช่เรื่องเดียว

4. **Auto เลือก engine ตอนยังไม่มี analysis** — demo compile plan ทันทีที่กด play ขณะ analysis ยังทำอยู่บน worker `route_auto` จึงตกไป fallback `Monophonic` ("no analysis available: WSOLA is the safe baseline") พอ analysis เสร็จบอกว่า `polyphonic tonal` ก็ไม่มีใครสั่ง re-plan **full mix จึงเล่นผ่าน WSOLA ทั้งเพลง** ซึ่งเป็นโหมดที่เอกสาร (research §8) บอกไว้เองว่าไม่เหมาะกับ polyphonic แก้ด้วยการ mark plan dirty เมื่อ analysis มาถึง ถ้า plan ที่กำลังเล่นถูก compile โดยไม่มี analysis
5. **WSOLA overshoot ทะลุ 0 dBFS** — คือต้นเหตุของ "ย่านต่ำแตก" จริง ๆ ทดสอบกับ master ที่ normalize ไว้ที่ peak 0.9886:

   | engine | peak ที่ +3 semitones |
   |---|---|
   | polyphonic | 0.8276 |
   | hybrid | 0.7424 |
   | **monophonic (WSOLA)** | **1.0104 — เกิน full scale** |

   overlap-add ของสอง segment ที่ correlate กันดีทำให้ peak โตกว่า input ได้ และ **ระบบไม่ normalize ให้โดยเจตนา** ตาม system-design §12 ผลคือ device clip และย่านต่ำซึ่งมีพลังงานมากที่สุดแตกก่อน

   ทางแก้ที่ไม่ขัดเอกสาร: **ไม่แอบ normalize** แต่ทำให้ผู้ใช้เห็นและแก้เองได้ — เพิ่ม peak/clip meter สดในสตรีม (`StreamMetrics::peak`, `clipped_frames`) และ **output trim (dB)** ที่ผู้ใช้คุมเอง วัด peak **ก่อน** trim เพื่อให้การหรี่ trim แก้เสียงแตกที่ device ได้โดยไม่ปิดบังว่า render ยังร้อนอยู่
6. **เสียงกระตุกมาจากสองที่พร้อมกัน**
   - WSOLA search แพงเกินไปใน callback: exhaustive scan คือ `(2·max_shift+1) × overlap × channels` ≈ 700k MAC ต่อเฟรม ตกอยู่ใน callback เดียว วัดได้ **max 7.61 ms จาก budget 10 ms** แก้เป็น coarse-to-fine (decimate หยาบก่อน แล้ว refine เต็มความละเอียดรอบผู้ชนะ) → bench p99 **1.023 → 0.163 ms**
   - lookup ที่เป็น O(n) ต่อ candidate: ไฟล์นี้ analyse ได้ **1157 onsets** และ `protections.iter().any(...)` ถูกเรียกทุก candidate เปลี่ยนเป็น binary search (`protects` / `protect_starts_in`) เช่นเดียวกับ `shift_limit_at` ที่เดิมวนทุก anchor ต่อเฟรม
   - **UI กิน CPU จนแย่งกับ audio thread**: `draw_wave` สแกนทุก sample ทุกครั้งที่ repaint ไฟล์ stereo 333 วินาที = **32 ล้าน sample ต่อเฟรม ที่ 30 fps** แก้ด้วย min/max peak cache 4096 bucket คำนวณครั้งเดียวตอนโหลด

## 6. Contract ที่บังคับใช้จริงในโค้ด

- **`process()` ไม่ allocate** — buffer ทั้งหมดจองใน `prepare()`; `InputRing`/`OutputAccum` เป็น linear buffer + compaction; ไม่มี `Vec` เกิดใหม่ใน loop
- **caller block size อะไรก็ได้** — driver ใน `engines/core.rs` consume prefix / produce prefix และ latch EOF เมื่อ input สุดท้ายถูก consume จริง; ทดสอบด้วย block 1, 17, 64, 127, 256, 1024, 4096 และ random
- **no-progress call ต้องบอกว่ารออะไร** — คืน `NeedInput`/`Draining` เสมอ; `render.rs` ตรวจ stall แล้ว error แทนที่จะ spin
- **หนึ่งการตัดสินใจต่อทุกแชนเนล** — WSOLA ใช้ offset เดียว (score ถ่วงพลังงาน ไม่ downmix), PV ใช้ instantaneous frequency ประมาณเดียว หมุนทุกแชนเนลด้วยมุมเดียวกัน, peak partition และ transient reset ใช้ร่วมกัน
- **ไม่ซ่อมเงียบ ๆ** — anchor ไขว้/ซ้ำ, internal ratio เกิน, formant บน engine ที่ไม่มีเส้นทาง, protection ที่ไม่ feasible, group ที่ไม่ตรง → typed error ก่อน render
- **ไม่ normalize อัตโนมัติ** — รายงาน peak/RMS/headroom ให้ผู้ใช้ตัดสินใจเอง

## 7. Demo (egui)

`cargo run --release --features demo --bin solfege-demo`

- โหลด WAV ด้วย path / drag-and-drop หรือเลือก fixture สังเคราะห์ 10 แบบ
- slider: alpha (log scale 0.25–4), transpose ±12 semitones, formant policy, quality, attack protect, block size, seed
- **คลิกบน waveform ต้นฉบับ = เพิ่ม anchor**; **ลาก anchor บน waveform ผลลัพธ์ = เลื่อนเวลา**; ปุ่ม "from onsets" promote onset ที่ตรวจพบทั้งหมดเป็น anchor
- กราฟ `W(s)` ด้านล่างแสดง time map เทียบเส้น identity พร้อม `alpha`, `p`, **internal ratio** และ `M`
- แถบล่างแสดง engine ที่ถูกเลือก + เหตุผลของ Auto, จำนวน frame ที่ได้เทียบกับที่คาด, peak/RMS, เวลา render และ error ที่ compiler ปฏิเสธ (เป็นข้อความเดียวกับ CLI)
- **warp (time map) toggle** — ปิดแล้ว render ด้วย identity map: transpose กับ formant ยังทำงาน แต่ไม่ยืดอะไรเลย · anchors ถูกเก็บไว้ เปิดกลับมาได้เหมือนเดิม · ต่างจาก Bypass ตรงที่ engine ยังทำงานอยู่
- **▶ live** สตรีมผ่าน engine ใน audio callback จริง; ขยับ slider แล้วเสียงเปลี่ยนทันทีโดยไม่หยุด (plan swap + crossfade) และ **▶ dry** สตรีม Bypass ไว้ A/B ที่ระดับเดียวกัน
- แถบล่างแสดง callback p99/max, underruns, starved frames, จำนวน swap, **output peak (dBFS) และจำนวน frame ที่ clip** และตำแหน่ง source แบบสด เป็นสีแดงทันทีที่มี underrun หรือ clipping
- **monitor trim (dB)** + **clip guard** (monitor เท่านั้น เปิดไว้เป็นค่าเริ่มต้น) — render ไม่ถูกแตะ meter รายงาน peak จริงก่อน trim
- waveform ใช้ min/max cache 4096 bucket ที่คำนวณครั้งเดียวตอนโหลด ไม่สแกนทั้งไฟล์ทุก repaint
- rate/channel ของ device ถูกจัดการใน `DeviceBridge` ฝั่ง demo ไม่ใช่ในไลบรารี (ไลบรารีไม่รู้จัก audio driver เลย)
- analysis และ render อยู่บน worker thread แยก UI ไม่ค้าง

## 8. สิ่งที่ยังไม่มี (พูดให้ตรงเหมือนเดิม)

| ยังไม่มี | หมายเหตุ |
|---|---|
| Note editing (M6) | `NoteEdit`, `DetectedNote`, F0+confidence, note segmentation มีครบในโครงสร้างและ analysis แล้ว แต่ **ยังไม่มี synthesis path ที่ใช้ NoteEdit** — ตอนนี้ validate/serialize ได้ แต่ render ยังไม่นำไปใช้ |
| ~~Real-time playback engine (M5)~~ | **ทำแล้ว** ดู §3.1 · ที่ยังขาดคือ disk-backed reader, loop และ live input |
| Multi-track group render | `check_group` validate ได้แล้ว แต่ยังไม่มี API ที่ render หลาย track ด้วย plan เดียวพร้อมกัน |
| Analysis/render cache บนดิสก์ | คีย์ (`analysis_key`, `render_key`) คำนวณแล้วและ deterministic; ยังไม่มีชั้น temp+atomic rename บนดิสก์ |
| Multi-resolution PV, sines-transients-noise, neural | อยู่ใน backlog ตามเอกสาร ไม่ได้แตะ |
| Listening test | **ยังไม่ได้ทำเลย** ไม่มีคำกล่าวอ้างเรื่องคุณภาพเสียงในหน้านี้ |
| การเทียบกับผลิตภัณฑ์เชิงพาณิชย์ | ยังไม่ได้ทำ และจะไม่อ้างจนกว่าจะใช้ corpus/protocol เดียวกัน |

## 9. ที่ที่ควรแก้ต่อ

1. **~~WSOLA search cost~~** — แก้แล้วด้วย coarse-to-fine (p99 0.163 ms) · ที่เหลือคือดูว่า coarse pass เลือก offset ต่างจาก exhaustive มากแค่ไหนในเนื้อเสียงจริง ซึ่งต้องฟัง
2. **Headroom policy** — ตอนนี้แค่ *รายงาน* peak ให้ผู้ใช้หรี่เอง ควรมี suggested trim จากผลวัด peak ของ render จริง (มี `metrics::level` อยู่แล้ว) ในชั้น export ด้วย
3. **Attack ที่ ratio สูง** — alpha 1.5 ยังได้ rise 4.59 ms / onset error 14.4 ms ทางแก้จริงคือ multi-resolution หรือ transient-aware frame placement ซึ่งทั้งคู่แตะ scheduler
4. **Formant** — envelope ใช้ moving average ใน log domain (ประมาณ 250 Hz) เป็น approximation ที่ตั้งใจให้หยาบ ต้องเทียบกับ fixture `vowel-*` ที่รู้ตำแหน่ง resonance ก่อนจะเรียกว่าใช้ได้
5. **Hybrid mask leakage** — `branch_energy()` มีให้แล้วแต่ยังไม่มี diagnostic ที่วัด leakage เป็นตัวเลข
6. **Note synthesis (M6)** — เส้นทางที่เหลือใหญ่ที่สุด
7. **Disk-backed reader + loop** — worker วางโครงไว้ให้เปลี่ยนได้ที่จุดเดียว แต่ตอนนี้ source อยู่ใน RAM ทั้งก้อน
8. **Segment-level mode switching** — ตอนนี้เลือก mode ต่อ clip ตามที่เอกสารกำหนดไว้สำหรับรุ่นแรก
