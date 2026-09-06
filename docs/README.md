# Solfege Stretching — Research & System Design

เอกสารภาษาไทยสำหรับศึกษาการยืดเวลาเสียงและออกแบบระบบของ `solfege-stretching` ตรวจค้นแหล่งข้อมูลวันที่ **6 กันยายน 2026**

## อ่านตามลำดับ

0. [Summary: สรุปทั้งชุดก่อนเริ่มงาน](summary.md) — ภาพรวมทุกไฟล์ในหน้าเดียว พร้อมข้อเท็จจริง workspace ที่ตรวจแล้ว และงานถัดไป (M0)
1. [Research: ผลิตภัณฑ์และโหมดย่อย](research.md) — Flex Time, Flex Pitch, élastique, Elastic Audio, AudioWarp และตัวเทียบสำคัญ
2. [DSP: กลไกและสมการ](dsp.md) — time map, resampling, OLA/WSOLA, PSOLA, phase vocoder, transient และ formant
3. [System Design](system-design.md) — สถาปัตยกรรม Rust, data model, processing contract และการจัดการ real-time
4. [Validation & Roadmap](validation.md) — ชุดทดสอบ เกณฑ์วัดคุณภาพ และลำดับพัฒนา
5. [Sources & Evidence](sources.md) — แหล่งต้นทาง รุ่นที่อ้างอิง และสิ่งที่ยังไม่ทราบ
6. [Implementation](implementation.md) — สิ่งที่สร้างแล้ว วิธีรัน ผลวัดจริง และสิ่งที่ยังไม่มี

## ขอบเขตและสถานะ

“เอาหมดทุกตัว” ในชุดนี้หมายถึง **ทุก reference ที่ระบุและทุกโหมดหลักที่เอกสารต้นทางที่ตรวจพบอธิบาย** พร้อมภาพรวมตระกูล DSP และตัวเทียบเพิ่มเติม ไม่ใช่คำรับรองว่ารวบรวมทุกผลิตภัณฑ์ ทุกเวอร์ชัน หรือทุกงานวิจัยที่เคยมี

คำว่า **elastic** ตีความเป็น **Avid Pro Tools Elastic Audio**; แยกจาก **zplane élastique** อย่างชัดเจน

| ป้าย | ความหมาย |
|---|---|
| Confirmed | เอกสารผู้ผลิตหรืองานวิจัยระบุโดยตรง |
| Inference | ข้อวิเคราะห์ของเรา ไม่ใช่ข้อมูลภายในของผลิตภัณฑ์ |
| Proposed | ข้อเสนอสำหรับ Solfege ยังไม่ได้ implement หรือ benchmark |
| Unknown | แหล่งที่ตรวจไม่เปิดเผย หรือมีข้อมูลไม่พอสรุป |

**อัปเดต 2026-09-06:** เอกสาร research/design ในชุดนี้ยังคงสถานะเดิม แต่โปรเจกต์ **ไม่ใช่ Hello World แล้ว** — ไลบรารี Rust, CLI, ชุดทดสอบ 36 ตัว และ demo egui ถูก implement ตามสัญญาในเอกสารเหล่านี้ รายละเอียดและผลวัดอยู่ใน [Implementation](implementation.md)

แนวทางคือ **Rust DSP ของเราเอง มีหลายโหมดภายใต้ time map เดียว** เริ่มจาก offline renderer และตัววัดคุณภาพ (ทำแล้ว) ก่อนเพิ่มการเล่นไฟล์แบบ real-time และการแก้โน้ตร้อง (ยังไม่ทำ) ไม่อ้างว่าให้คุณภาพเทียบเท่าผลิตภัณฑ์เชิงพาณิชย์จนกว่าจะทดลองฟัง ซึ่ง **ยังไม่ได้ทำ**
