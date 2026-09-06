# Research: Time Stretching & Pitch Editing

ตรวจค้น 2026-09-06 · [สารบัญ](README.md) · [ทะเบียนหลักฐาน](sources.md)

## 1. สิ่งที่กำลังเปรียบเทียบ

Time stretching เปลี่ยนระยะเวลาโดยพยายามรักษา pitch; pitch shifting เปลี่ยน pitch โดยรักษาระยะเวลา; varispeed เปลี่ยนทั้งสองตามอัตราเล่น ส่วน warp เป็นการกำหนดว่าจุดใดของไฟล์ต้องไปอยู่เวลาใด ไม่ใช่ชื่ออัลกอริทึมสังเคราะห์เสียง

| Reference | ประเภท | หน้าที่หลัก | ความสัมพันธ์ |
|---|---|---|---|
| Flex Time | ระบบแก้ timing ของ Logic Pro | marker และเลือกวิธี stretch | มีหลาย algorithm |
| Flex Pitch | ระบบแก้โน้ตของ Logic Pro | pitch, timing, vibrato, formant | ต้องมีการวิเคราะห์โน้ต |
| élastique | DSP SDK ของ zplane | time/pitch processing | DAW อื่นนำไปใช้ได้ |
| Elastic Audio | framework ของ Pro Tools | analysis, warp, real-time/rendered | มี élastique เพิ่มตั้งแต่ 2023.3 |
| AudioWarp | workflow ของ Cubase/Nuendo | tempo matching และ free warp | เลือก élastique หรือ Standard |

อ้างอิง: [Apple](sources.md#a1), [zplane](sources.md#z1), [Avid](sources.md#p2), [Steinberg](sources.md#s3)

## 2. Logic Pro — Flex Time

**Confirmed:** มีหกโหมดด้านล่าง และ Automatic ซึ่งเลือก Monophonic, Slicing หรือ Polyphonic ตามการวิเคราะห์ ไม่ใช่เครื่องยนต์ที่เจ็ด รายการอ้างเอกสาร Apple; ข้อจำกัดเสียงในคอลัมน์สุดท้ายเป็น **Inference** จากกลไก ไม่ใช่ผลฟังทดสอบ [A1](sources.md#a1)

| โหมด | กลไกที่เปิดเผย | ตัวควบคุม | งานเหมาะสม / จุดเสี่ยง |
|---|---|---|---|
| Slicing | ตัดที่ transient แล้วย้าย slice โดยไม่เปลี่ยนอัตราเล่นภายใน | Fill Gaps, Decay, Slice Length | กลอง; อาจเหลือช่องว่างหรือตัดหาง |
| Rhythmic | ใช้ loop ระหว่าง slice เติมช่วงขยาย | Loop Length, Decay, Loop Offset | rhythm guitar/keys; loop อาจฟังซ้ำ |
| Monophonic | สำหรับแนวทำนองเดี่ยว; ไม่เปิดเผยสูตรแกนกลาง | Percussive ปกป้องบริเวณ transient | ร้อง/เบสแห้ง; reverb ทำให้แยกเสียงยาก |
| Polyphonic | Apple ระบุ phase vocoding | Complex เพิ่ม internal transients | chord/mix; phase และ attack เป็นโจทย์สำคัญ |
| Tempophone (FX) | เล่น/ซ้ำ grain ที่ความเร็วเดิมและ crossfade | Grain Size, Crossfade | effect; ความเป็น grain เป็นส่วนของเสียง |
| Speed (FX) | เปลี่ยนความเร็วเล่นพร้อม pitch | ตามการเปลี่ยน timing | tape effect; รักษาคีย์ไม่ได้ |

**Unknown:** FFT size, window, phase-lock rule, transient detector และวิธี Monophonic ภายใน ไม่ควรเรียกทุกโหมดว่า phase vocoder หรือยืนยันว่า Monophonic = PSOLA

**บทเรียนสำหรับเรา:** แยกโหมดรักษาเสียงต้นฉบับออกจาก FX และมีโหมดเลื่อน attack โดยไม่ยืดตัว attack

## 3. Logic Pro — Flex Pitch

**Confirmed:** เป็นการแก้ pitch แบบมีโน้ตเป็นหน่วย รวมการย้ายและปรับความยาวโน้ต จุดควบคุมหกจุดคือ pitch drift ต้น/ท้าย, vibrato, gain, fine pitch และ formant shift มี Formant Track กับ Formant Shift ระดับ track ด้วย [A2–A3](sources.md#a2)

**Inference — แบบจำลองเพื่อเข้าใจ:** วิเคราะห์ contour → แบ่งโน้ต → เก็บการแก้ไขแยกจากเสียง → สังเคราะห์ตามเวลาและ pitch ใหม่ เส้นทางนี้อธิบายความต้องการของระบบได้ แต่ไม่ใช่คำยืนยันว่า Apple ใช้ F0 estimator, PSOLA หรือ spectral-envelope estimator ชนิดใด

ควรแยก **monophonic content** (ทำนองเดียว) จาก **mono channel** (แชนเนลเดียว) เสียงร้อง stereo อาจมีทำนองเดียว และไฟล์ mono อาจมีทั้งวง ระบบของเราจะเลือกวิธีจากเนื้อเสียง ไม่ใช่จำนวนแชนเนลอย่างเดียว

**Unknown:** การแยกเสียงไม่ก้อง/เสียงก้อง, การแบ่ง voiced/unvoiced และการแก้ octave error ภายใน Apple ข้อเสนอของเราให้แสดง confidence และให้ผู้ใช้แก้ขอบโน้ตได้

## 4. zplane — élastique

### 4.1 ตระกูลและชื่อที่ต้องแยก

| ตัว | สิ่งที่เปิดเผย | ขอบเขต |
|---|---|---|
| PRO | งานทั่วไป, transient preservation, inter-channel coherence, formant preservation | เครื่องยนต์คุณภาพสูงตามคำผู้ผลิต |
| EFFICIENT | tonal/transient processing ที่ลดภาระคำนวณ; มีการแบ่งงานเพื่อลด workload peak | งาน polyphonic/real-time ที่จำกัด CPU |
| SOLOIST / monophonic modes | Pro SDK 3.x มีโหมด monophonic; บันทึกว่ามี speech mode กลับมาใน 3.2.1 | ไม่ใช่ระบบแยกโน้ต polyphonic |
| TUNE | วิเคราะห์เป็น pitch objects แล้วแก้ทำนอง/รายโน้ต; แยก analysis/synthesis | ใกล้ชั้น note editor มากกว่า raw streaming stretcher |

แหล่ง: [Z1](sources.md#z1), [Z2](sources.md#z2) หน้าเว็บ zplane กล่าวรวมว่าทั้งตระกูลทำงาน real-time แต่ตารางระบุ TUNE ไม่รองรับ real-time time stretching แบบเดียวกับ PRO/EFFICIENT จึงตีความได้เพียงว่า **สังเคราะห์หลังวิเคราะห์ได้แบบ real-time** ไม่รับรอง raw live analysis โดยไม่มี lookahead

### 4.2 กลไกที่ยืนยันได้

SDK อธิบายการทำ pitch shift ด้วย **time stretcher + resampler** และผลต่างด้าน timing เมื่อ resampler เปลี่ยนอัตราทันทีแต่ stretch transition ค่อยเปลี่ยน มี pitch synchronization และข้อแลกเปลี่ยนเรื่อง buffer/response time [Z3](sources.md#z3)

Pro SDK อธิบาย infiniStretch/Hold ด้วย frequency-frame extrapolation; Hold เลือกได้ว่าจะหยุด source position หรือปล่อย timeline เดินต่อ ข้อมูลนี้ไม่เพียงพอสร้าง PRO ทั้งตัวขึ้นใหม่ [Z2](sources.md#z2)

**Unknown:** psychoacoustic model, spectral partition, window schedule และ transient synthesis ภายใน คำโฆษณาว่าไม่มี artifact ไม่ใช่ benchmark และเราไม่ใช้เป็นข้อสรุปด้านคุณภาพ

**บทเรียนสำหรับเรา:** ทำ API ที่รายงาน input consumed/output produced จริง เก็บ source clock กับ presentation clock แยกกัน และให้ pitch automation ใช้ clock ที่นิยามชัด

## 5. Avid Pro Tools — Elastic Audio

ระบบระดับ track เลือก engine และวิธี real-time หรือ rendered มี analysis ก่อนแก้ไข รายการ legacy ใช้คู่มือ Pro Tools First 12.0 เป็นฐาน และเสริมหลักฐานการเพิ่ม élastique ใน Pro Tools 2023.3; ไม่ถือว่าคู่มือเก่าคือรายการรับรองทุก edition ในปี 2026 [P1–P2](sources.md#p1)

| Engine | กลไก/พฤติกรรมที่เปิดเผย | หมายเหตุ |
|---|---|---|
| Polyphonic | ทั่วไปสำหรับหลายเครื่องดนตรี; รองรับ clip pitch shift | สูตร DSP ไม่เปิดเผย [P3](sources.md#p3) |
| Rhythmic | เหมาะ attack ชัด เช่น drums; รองรับ clip pitch shift | อย่าอนุมานว่าเหมือน Logic Slicing [P4](sources.md#p4) |
| Monophonic | วิเคราะห์ pitch เพิ่มจาก peak transients | คู่มือฐานระบุไม่รองรับ Elastic Audio pitch shifting แม้วิเคราะห์ pitch [P5](sources.md#p5) |
| Varispeed | ผูก pitch กับเวลาแบบเทป | ไม่มี independent pitch [P6](sources.md#p6) |
| X-Form | ดัดแปลงจาก X-Form AudioSuite ที่ใช้ iZotope Radius | Elastic Audio ตัวนี้ rendered-only; ไม่ใช่ standalone AudioSuite ตัวเดียวกัน [P7](sources.md#p7) |
| élastique Pro V3 | zplane engine เพิ่มใน 2023.3; real-time และ formant preservation | Avid ระบุ timing/coherence/sample accuracy; ยังไม่ใช่ผลทดสอบของเรา [P2](sources.md#p2) |

**Inference:** คุณภาพไม่ได้ขึ้นกับชื่อ engine อย่างเดียว; transient analysis, warp constraints, buffer และ render policy ของ host เป็นส่วนหนึ่งของผลลัพธ์ จึงไม่จัดอันดับ DAW จากชื่อ engine

**บทเรียนสำหรับเรา:** analysis event ไม่ควรกลายเป็น user anchor ทุกจุด; ต้องแยกข้อมูลตรวจพบกับข้อกำหนดที่ผู้ใช้ล็อก

## 6. Steinberg Cubase/Nuendo — AudioWarp

### 6.1 ชั้น workflow

**Confirmed:** Musical Mode ให้ audio ตาม tempo; Auto/Manual Adjust กำหนด grid; Free Warp ย้ายจุดเฉพาะ โดยมี warp algorithm อยู่ข้างใต้ [S3](sources.md#s3)

Phase-Coherent AudioWarp ใช้ร่วมกับ Group Editing สำหรับหลายไมค์ คู่มือกำหนด events เริ่ม/จบตรงกันและ marker อยู่ตำแหน่งเดียวกัน [S4](sources.md#s4) การเลือก marker ร่วมอย่างเดียวจึงไม่พอจะรับรอง phase coherence ของ implementation ใหม่

### 6.2 élastique presets

มี PRO, PRO Formant และ efficient แต่ละตัวมี **Time / Pitch / Tape** รวมเก้า combination: Pro-Time, Pro-Pitch, Pro-Tape; Pro-Formant-Time, Pro-Formant-Pitch, Pro-Formant-Tape; efficient-Time, efficient-Pitch, efficient-Tape [S1](sources.md#s1)

Time ให้ความสำคัญกับ timing accuracy; Pitch ให้ความสำคัญกับ pitch accuracy; Tape ผูกสองอย่างแบบเปลี่ยนความเร็วเทป ชื่อ Time/Pitch ไม่ได้หมายความว่าทำอีกอย่างไม่ได้ หรือว่าเป็นคนละตระกูล DSP ทั้งหมด

### 6.3 Standard presets

| Preset | เหมาะกับ |
|---|---|
| Drums | percussion; tuned percussion อาจต้องลอง Mix |
| Plucked | attack ตามด้วยเนื้อเสียงค่อนข้างคงที่ |
| Pads | tonal sustain; ยอมลดความแม่นของ rhythm |
| Vocals | เนื้อเสียง tonal และ transient ในจังหวะช้า |
| Mix | pitched material ที่ซับซ้อนกว่า |
| Custom | ปรับ Grain Size, Overlap, Variance |
| Solo | monophonic material และรักษา timbre |

**Confirmed:** Standard เน้น CPU-efficient realtime และเอกสารเปิดเผยการแบ่งเป็น grain; ไม่เปิดเผยว่าทุก preset ใช้สูตรเดียวกัน Variance เพิ่มการแปรตำแหน่งแลกกับ rhythmic smearing [S2](sources.md#s2)

VariAudio เป็นการแก้โน้ตอีกชั้น; คู่มือ algorithm presets ระบุ Standard–Solo ถูกใช้โดยอัตโนมัติสำหรับ VariAudio warping/pitching จึงไม่เท่ากับการเลือก élastique เพื่อ warp clip [S5](sources.md#s5)

## 7. ตัวเทียบเพิ่มเติม

### Ableton Live 12 Warp

**Confirmed:** Beats รักษา transient และควบคุมช่วง/ซองเสียงระหว่าง hit; Tones ปรับ grain ตามลักษณะ pitch; Texture มี Grain Size/Fluctuation; Re-Pitch เปลี่ยนอัตราเล่น; Complex และ Complex Pro เหมาะ signal ผสม โดย Pro มี Formants/Envelope [B1](sources.md#b1)

**Unknown:** คู่มือที่ตรวจไม่ระบุ backend version ของ Complex ใน Live 12 จึงไม่ใช้รายชื่อลูกค้า Live 5–9 ของ zplane ยืนยันว่า Live 12 ทุกโหมดใช้ élastique

### Rubber Band R2 / R3

**Confirmed:** R2 เปิดเผยว่าเป็น block phase vocoder มี transient phase reset, adaptive ratio และ phase lamination; pitch shift ใช้ resampling ร่วมกับ stretching ข้อความนี้จำกัดที่ R2 [R1](sources.md#r1)

R3/Finer เป็น engine แยก ผู้พัฒนาระบุคุณภาพดีกว่าในหลายเนื้อเสียงแต่ใช้ CPU มากกว่า ไม่เอารายละเอียด R2 ไปอธิบาย R3 แบบยืนยัน [R2](sources.md#r2) Integration guide แสดงชัดว่าแต่ละ call ให้จำนวน output ไม่คงที่ [R3](sources.md#r3)

### SoundTouch

**Confirmed:** WSOLA-like ใน time domain ร่วมกับ rate transposition; tempo, rate และ pitch เป็นการประกอบสองหน่วยนี้ จึงเป็น baseline ที่อธิบายง่ายสำหรับ streaming ข้อความ latency ราว 100 ms ใน README เป็นตัวอย่างของไลบรารี ไม่ใช่ขีดจำกัดของ WSOLA ทั้งตระกูล [T1](sources.md#t1)

### Melodyne 5

**Confirmed:** Universal สำหรับ signal ผสม; Percussive สำหรับเสียงไม่ชัด pitch; Percussive Pitched สำหรับกลองมี pitch; Melodic สำหรับทำนองเดี่ยว; Polyphonic Sustain/Decay ใช้ DNA แก้โน้ตในคอร์ดตามลักษณะ sustain/attack ความสามารถขึ้นกับ edition และ detection ต้องแก้ได้ [M1](sources.md#m1)

การแก้โน้ตหนึ่งตัวในคอร์ดต่างจากการ pitch shift ทั้งไฟล์; การมี polyphonic stretcher ไม่ได้ทำให้มี DNA โดยอัตโนมัติ

### Zynaptiq ZTX

ผู้ผลิตระบุ adaptive wavelet technology มี Core/FX/Retune API สำหรับงานต่างกัน รวม formant และ multichannel phase locking แต่ไม่เปิดเผยสูตรเพียงพอทำซ้ำ และคำอ้างว่าเหนือกว่า PV/PSOLA ยังต้องพิสูจน์ด้วย corpus เดียวกัน [X1](sources.md#x1)

### Serato Pitch ’n Time

รองรับ variable tempo/pitch maps, Time-Morph และ multichannel processing ตามเอกสารผู้ผลิต แกนกลางเป็น proprietary; หน้า feature ไม่ใช่หลักฐานว่าเป็น WSOLA/PV แบบใด เหมาะเป็นตัวเทียบ workflow กับผลฟังมากกว่า implementation reference [X2](sources.md#x2)

### Paulstretch

โค้ดตัวอย่างผู้เขียนใช้ magnitude FFT แล้วสุ่ม phase ก่อน inverse FFT และ overlap-add เป็นการสร้างเนื้อเสียงยาวเชิงสร้างสรรค์ การทิ้ง phase ทำให้การรักษาหัวเสียงและตำแหน่งเชิงจังหวะไม่ใช่คุณสมบัติหลัก ไม่ควรใช้เป็น baseline ของงาน quantize drums [X3](sources.md#x3)

## 8. Coverage matrix สำหรับการตัดสินใจ

ตารางนี้เป็น **Inference/ข้อเสนอการเลือกใช้งาน** ไม่ใช่ ranking จากการฟัง ทุกกลุ่มที่มีหลายโหมดต้องเลือกโหมดให้เหมาะก่อนเทียบ

| ปัญหา | แนวทางที่ควรทดลองก่อน | ตัวอ้างอิง | สิ่งที่ต้องฟัง |
|---|---|---|---|
| กลองหลายไมค์ | shared slicing/transient-aware warp | Flex Slicing, AudioWarp group | double attack, comb filtering |
| เบส/ร้องแห้ง | WSOLA หรือ pitch-aware synthesis | Monophonic, SOLOIST, Melodic | octave errors, flutter |
| เปียโน/กีตาร์คอร์ด | phase-coherent spectral หรือ hybrid | Flex Polyphonic, élastique | phasiness, attack smear |
| full mix | harmonic/percussive hybrid | élastique, Complex Pro, R3 | cymbal wash, stereo image |
| แก้ pitch รายโน้ต | analysis + note graph + synthesis | Flex Pitch, TUNE, VariAudio | consonants, transitions, formant |
| เปลี่ยนความเร็วเทป | anti-aliased resampling | Speed, Tape, Re-Pitch | aliasing และ intentional pitch |
| drone ยาวมาก | granular/spectral hold | Tempophone, Paulstretch | continuity/texture ตามเจตนา |

## 9. ข้อสรุปเพื่อออกแบบ

**Proposed:** สร้าง time-map/anchor layer ก่อน แล้วมี Bypass, Tape, Percussive, Monophonic, Polyphonic, Hybrid และ Texture แยกกัน; Auto เป็น routing policy ที่ผู้ใช้ override ได้ ไม่ต้องมีอัลกอริทึมเดียวที่ทำทุกอย่าง

คุณสมบัติที่ต้องออกแบบเป็นระบบตั้งแต่ต้นคือ exact output length, linked-channel decisions, seek/reset, analysis cache, note confidence และ automation clock รายละเอียดอยู่ใน [System Design](system-design.md)

ยังไม่มี audio A/B, CPU benchmark หรือการตรวจ source code ภายในผลิตภัณฑ์ proprietary ในงานนี้; ทุก quality claim ถูกจำกัดตามหลักฐาน ไม่สรุปว่า engine ใดดีที่สุด
