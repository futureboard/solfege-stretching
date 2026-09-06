# DSP: กลไกที่ใช้สร้างระบบได้

[สารบัญ](README.md) · เอกสารส่วนนี้แยก **ความรู้กลไกสาธารณะ** กับ **Proposed implementation** ของเรา ไม่ใช่ reverse engineering ผลิตภัณฑ์ที่อ้างอิง

## 1. นิยามเวลาและ pitch

กำหนด sample rate ภายในเท่ากันทั้ง input/output; การแปลง sample rate ของไฟล์เป็นอีกขั้นหนึ่ง

- `s` = ตำแหน่ง input หน่วย sample frames (หนึ่ง frame มีทุกแชนเนล)
- `t = W(s)` = ตำแหน่ง output; `W` คือ forward time map
- `alpha = dt/ds` = duration ratio; `alpha > 1` ยาวขึ้น/ช้าลง
- `v = ds/dt = 1/alpha` = playback speed
- `p = 2^(semitones/12)` = pitch multiplier; `p = 2` สูงขึ้นหนึ่ง octave
- `f = 2^(formant_semitones/12)` = spectral-envelope multiplier; เป็นตัวควบคุมแยกจาก `p`

ตัวอย่าง: เสียง 10 วินาที 120 BPM → 90 BPM ให้ `alpha = 120/90 = 4/3` และ output 13⅓ วินาที ถ้ารักษา pitch ให้ `p = 1`; ถ้า Tape ให้ `p = 1/alpha = 0.75` หรือประมาณ −4.98045 semitones

สำหรับ source ที่มี tempo เปลี่ยน ต้อง map ผ่านตำแหน่ง beat ไม่ใช้ BPM เดียวทั้งไฟล์:

```text
source sample -> source beat map -> destination beat/time map -> output sample
```

ตำแหน่ง sample เป็น source of truth ใน DSP ส่วน beats อยู่ชั้น musical document

## 2. ภาพรวมตระกูล

งาน review และ TSM Toolbox แสดง OLA, WSOLA, phase vocoder และ HPSS hybrid พร้อม trade-off ว่า tonal continuity กับ transient sharpness ต้องการวิธีต่างกัน [D1–D2](sources.md#d1)

| วิธี | ทำงานอย่างไร | ปัญหาหลัก | บทบาทในเรา |
|---|---|---|---|
| Resampling | อ่าน waveform ด้วยอัตราใหม่และ interpolation/filter | pitch เปลี่ยนตามเวลา | Tape และส่วนประกอบ pitch shift |
| Slicing | ย้ายชิ้นเสียงโดยรักษาความเร็วภายใน | gap/overlap/tail | drums |
| OLA | ย้ายและซ้อน frame พร้อม window | waveform ต่อกันไม่ลง phase | reference baseline |
| SOLA/WSOLA | เลือก offset ให้ waveform ช่วงซ้อนคล้ายกัน | drift, echo, polyphonic mismatch | Efficient/Monophonic baseline |
| PSOLA/epoch-based | วางชิ้นเสียงตาม pitch periods/epochs | F0/epoch ผิดเกิด buzz/octave jump | ทางเลือก mono ในอนาคต |
| Phase vocoder | STFT และสะสม phase ตามเวลาสังเคราะห์ | phasiness และ transient smear | Polyphonic |
| Multi-resolution | ใช้หลายขนาดหน้าต่างตามเวลา/ความถี่ | cross-band consistency และ CPU | optimization หลัง baseline |
| HPSS/hybrid | แยก harmonic/percussive แล้ว stretch ต่างวิธี | mask leakage, branch alignment | Hybrid experimental |
| Sines–transients–noise | แยกองค์ประกอบแล้วสร้างใหม่แต่ละชนิด | model mismatch โดยเฉพาะ noise | research backlog |
| Granular / randomized spectral | ซ้ำ/กระจาย grain หรือสุ่ม phase | เปลี่ยน texture ตั้งใจ | Texture FX |
| Neural resynthesis | โมเดลสร้างเสียงจาก conditioning ที่ปรับเวลา | identity/detail อาจเปลี่ยนและประเมินยาก | ไม่อยู่ MVP |

หลักฐานเสริม: [WSOLA-like](sources.md#t1), [epoch-based research](sources.md#d3), [noise/neural research](sources.md#d4), [Paulstretch source](sources.md#x3)

## 3. Proposed: resampling และ independent pitch

Tape ใช้ `y[t] = interpolate(x, W^-1(t))`; ต้องมี anti-alias low-pass ที่สัมพันธ์กับอัตราอ่าน โดยเฉพาะช่วงอ่านเร็วขึ้น ใช้ fractional source position ต่อเนื่อง ไม่ปัดเป็น sample integer ทุกครั้ง

สำหรับ **constant** `alpha,p`: stretch ก่อนด้วย `alpha_internal = alpha * p` แล้วอ่าน intermediate signal ที่ rate `p` ทำให้ duration สุดท้าย `alpha_internal/p = alpha` และ pitch คูณ `p` ตัวอย่าง +12 semitones ที่เวลาเดิม: stretch 2 เท่าแล้ว resample ให้สั้นครึ่งหนึ่ง

สำหรับ **variable** automation ห้ามนำ `alpha*p` ไปใช้กับคนละ clock แบบตรง ๆ กำหนด intermediate coordinate `u(t)`:

```text
u(t) = integral[0..t] p(q) dq
U(s) = u(W(s))
U'(s) = p(W(s)) * W'(s)
z = stretch(x, source_to_intermediate = U)
y(t) = resample(z, position = u(t))
```

ใช้ numerical integration และ fractional accumulator เดียวที่ compile จาก automation ใน output time; ทุก anchor ต้องตรงหลัง resampling ด้วย การเลื่อน control ด้วย latency ต้องทำตาม logical timestamp ไม่ใช่เวลาที่ callback มาถึง

นี่เป็นแบบทางคณิตศาสตร์ของเรา; zplane และ Rubber Band ยืนยันเพียงแนวทางประกอบ stretch/resampler และความสำคัญของ synchronization [Z3](sources.md#z3), [R1](sources.md#r1)

## 4. Proposed: OLA/WSOLA baseline

เลือก analysis center `s_m` และ synthesis center `t_m`; ใช้ overlap-add พร้อม normalization:

```text
y[n] = sum_m (wa[n-t_m] * ws[n-t_m] * x[s_m+n-t_m])
       / max(epsilon, sum_m wa[n-t_m] * ws[n-t_m])
```

ถ้า apply window เดียวให้ใช้ตัวหารผลรวม window เดียว ห้ามใช้ผลรวมกำลังสองโดยไม่ดู analysis/synthesis chain จริง

WSOLA เลื่อน `s_m` ภายในช่วงค้นหา `±D` รอบตำแหน่งที่ map ต้องการ เพื่อ maximize normalized cross-correlation กับ waveform ที่จะซ้อน ไม่ normalize ด้วยพลังงานใกล้ศูนย์; silence ให้ใช้ตำแหน่ง nominal

ข้อกำหนดของเรา:

1. ใช้ offset เดียวทุกแชนเนล; score เป็นผลรวม correlation ที่ถ่วงด้วยพลังงานแชนเนล ไม่ downmix แล้วปล่อย L/R หักล้าง
2. จำกัด displacement และแก้ drift ก่อนถึง anchor ถัดไป โดยไม่เปลี่ยนตำแหน่ง hard anchor
3. ห้าม candidate ข้าม protected transient หรืออ่านพ้น buffer
4. ใช้ crossfade ที่ overlap มีข้อมูลครบ; ที่ขอบไฟล์ใช้ padding ที่นิยามชัด
5. โหมดนี้ไม่ต้องใช้ pitch detector; จะเพิ่ม pitch hints ได้แต่ไม่ถือว่าเป็น PSOLA

ค่าทดลองที่ 48 kHz: frame 20–40 ms, overlap 5–15 ms, search ±5 ms; เป็นจุดเริ่มต้น tuning ไม่ใช่ค่าที่รับรองคุณภาพ โดยเฉพาะเสียงเบส period ยาวต้องทดสอบต่างหาก

## 5. Proposed: phase vocoder baseline

ใช้ STFT `X_m[k] = A_m[k] exp(j phi_m[k])`, analysis hop จริง `Ha_m = s_m-s_(m-1)` และ synthesis hop `Hs_m = t_m-t_(m-1)`:

```text
omega_k = 2*pi*k/N
delta_m[k] = principal_arg(phi_m[k] - phi_(m-1)[k] - omega_k*Ha_m)
omega_hat_m[k] = omega_k + delta_m[k]/Ha_m
theta_m[k] = theta_(m-1)[k] + omega_hat_m[k]*Hs_m
Y_m[k] = A_m[k] * exp(j*theta_m[k])
```

`Ha_m` ต้องมากกว่าศูนย์ สำหรับ freeze ห้ามเอา `Ha=0` เข้าสมการนี้; freeze เป็น state แยก หรือใช้ phase increments ที่ประมาณจาก frame ก่อนหน้า

เริ่มด้วยหน้าต่าง 2048 samples และ analysis hop 512 ที่ 48 kHz; ตรวจ reconstruction ก่อนปรับคุณภาพ ใช้ adaptive frame placement/normalized synthesis เพื่อไม่เกิดช่องว่างเมื่อ synthesis hop ใหญ่เกิน window support อย่ารองรับ ratio สูงด้วยการเพิ่ม hop ไปเรื่อย ๆ อย่างเดียว

ขั้นเพิ่มคุณภาพ:

- **Peak phase locking:** peak bins เป็น reference ให้ bins รอบข้างรักษา phase offsets ลดการแตกความสัมพันธ์ภายใน partial
- **Transient handling:** ปกป้อง attack แล้วกระจายการยืดไป sustain; phase reset ไม่ทำอิสระทุกแชนเนล
- **Resolution:** window ยาวช่วย bass แต่ทำให้ตำแหน่ง attack คลุมเครือ ทดลอง multi-resolution หลังวัด baseline
- **Silence/DC/Nyquist:** phase ใน bin พลังงานต่ำไม่เสถียร; DC และ Nyquist ของ real FFT ต้องรักษาข้อจำกัด real-valued

**Implement แล้ว** พร้อม fractional analysis position, transient detection ในตัว, adaptive window และการยืนยัน COLA — ดู [Implementation §3.2](implementation.md) · สูตรนี้เป็น public-method baseline; ไม่อ้างว่าเท่ากับ Flex Polyphonic, élastique หรือ Rubber Band ข้อมูล R2 ที่ผู้พัฒนาเปิดเผยแสดงว่ารายละเอียด transient/coherence มีความสำคัญนอกเหนือจากสูตร PV พื้นฐาน [R1](sources.md#r1)

## 6. Proposed: transient-constrained mapping

แต่ละช่วงระหว่าง anchors ยาว `L_in -> L_out`; ถ้าปกป้อง attack รวม `P` frames แบบไม่ยืด ให้ stretch ส่วนที่เหลือด้วย:

```text
alpha_sustain = (L_out - P) / (L_in - P)
```

ใช้ได้เมื่อ `L_in > P` และ `L_out > P` เท่านั้น ตัวอย่าง 100 → 150 ms, attack 10 ms ให้ sustain ratio `140/90 ≈ 1.5556` ขณะที่ attack ratio = 1

ถ้าช่วง output สั้นกว่าผลรวม protected attacks ต้องส่ง `ConstraintConflict` หรือให้ผู้ใช้เลือกยอมลด protection ห้ามทำ negative ratio หรือตัด hit ทิ้งเงียบ ๆ เมื่อ windows ปกป้อง overlap กัน ให้ merge ก่อนคำนวณ

Slicing ใช้ shared cut points แล้วเติมช่องว่างด้วย tail loop/crossfade หรือ fade-to-silence ตาม policy; อย่าเอาช่วง pre-attack ของ hit ถัดไปมาวน การบีบเวลาให้ crossfade หางก่อน attack ถัดไปโดยคงตำแหน่ง attack

## 7. Proposed: harmonic/percussive hybrid

prototype แยกด้วย median statistics ใน spectrogram: แนวเวลาเพื่อ harmonic, แนวความถี่เพื่อ percussive จากนั้นสร้าง soft masks `Mh+Mp=1` ใช้ masks เดียวทุกแชนเนลจาก energy aggregate

```text
x -> shared STFT -> harmonic mask -> phase vocoder ---+
                  percussive mask -> transient path -+-> align -> sum
```

ทั้งสอง branch ใช้ hard anchors/output length เดียวกัน ต้องชดเชย **delay จริงของแต่ละ branch** ก่อนรวม การใช้ map เดียวกันอย่างเดียวไม่พอ หาก mask มีเสียง snare รั่วเข้า harmonic branch ยังเกิด smear ได้ จึงต้องฟังและวัดก่อนเลื่อนเป็น default [แนวทาง HPSS สาธารณะ](sources.md#d2)

MVP ไม่ใส่ random-phase noise branch ใน Natural mode; เพราะอาจเปลี่ยน stereo texture และไม่รักษาต้นฉบับ

## 8. Proposed: note pitch และ formants

เริ่มด้วย monophonic analysis: F0 candidates + confidence + voiced/unvoiced + boundaries; ใช้ continuity constraint ลด octave jumps; เปิดให้แก้ detected notes โดยไม่แก้ source samples

แยก pitch contour ในหน่วย cents เป็น `note_center + slow_drift + vibrato + residual` การแยกนี้เป็น model approximation ต้องกำหนด smoothing bandwidth และวิธีรักษา transition ทดลองกับ vibrato/glissando จริง ไม่ flatten ทุกอย่างเป็น MIDI note

Formant คือ envelope ของ spectral resonances ไม่ใช่ fundamental frequency สมมติ magnitude `A(omega)=E(omega)*R(omega)` การย้าย pitch ที่ไม่รักษา envelope จะได้ประมาณ `E(omega/p)`; ถ้าต้องการ formant scale `f` ให้ชดเชยด้วย:

```text
gain(omega) = E(omega/f) / max(E(omega/p), epsilon)
```

นี่เป็น **แบบประมาณเพื่อทดลอง** ไม่ใช่สูตรสมบูรณ์ของเสียงร้อง ใช้ log-envelope smoothing, gain clamp และไม่ boost bin เงียบรุนแรง เริ่มกับ voiced mono เท่านั้น; polyphonic envelope correction ไม่เท่ากับรักษา vocal tract ของนักร้องทุกคนใน mix

เสียง sibilants/breath ไม่ควรถูกบังคับมี F0: ให้ bypass pitch operation แต่ warp เวลาอย่างต่อเนื่องและ crossfade ขอบ voiced/unvoiced การรักษา formant ของเรา: `Preserve => f=1`, `FollowPitch => f=p`, `Shift(st) => f=2^(st/12)` ไม่ยืมความหมายตัวเลขจาก UI ผู้ผลิตอื่น

## 9. สิ่งที่ยังต้องทดลอง

PSOLA/ESOLA ต้องมี epoch accuracy ที่ดีกว่าแค่ F0 เป็นราย frame; noise resynthesis และ neural stretching มีงานวิจัยรองรับแนวคิดแต่ยังไม่ใช่คำตอบที่ทดแทน original waveform ได้ทุกงาน [D3–D4](sources.md#d3)

เส้นแบ่งสำคัญ: **ทำให้ยาวตามต้องการ**, **คง pitch**, **รักษาหัวเสียง**, **รักษาเฟส**, และ **ฟังเป็นธรรมชาติ** เป็นคนละคุณสมบัติ ต้องมี test แยกกันตาม [Validation](validation.md)
