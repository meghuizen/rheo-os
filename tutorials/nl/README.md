# Bouw je eigen besturingssysteem

Een boek voor jonge programmeurs

Welkom! In dit boek leer je stap voor stap hoe je een **besturingssysteem**
(Engels: *operating system*, afgekort OS) bouwt. We beginnen helemaal bij het
begin: hoe een computer aangaat, en hoe jij de allereerste code schrijft die de
computer uitvoert. Dat noemen we een **bootloader**.

Je hoeft geen expert te zijn. Als je een klein beetje kunt programmeren en je
durft dingen uit te proberen, dan kun je dit. We leggen alles in gewone taal
uit. Moeilijke woorden krijgen altijd eerst een uitleg.

## Voor wie is dit boek?

Voor jou, als:

- je nieuwsgierig bent hoe een computer echt werkt, van binnen;
- je al eens een programma hebt geschreven (in welke taal dan ook);
- je zin hebt om iets te bouwen dat de meeste mensen als "magie" zien.

Je hoeft **geen** dure computer of speciale hardware te kopen. We gebruiken een
gratis programma dat een computer *nadoet* op jouw eigen laptop. Dat heet een
**emulator**. Zo kun je veilig oefenen: als je iets fout doet, crasht alleen de
nagedane computer, niet die van jou.

## Wat ga je leren?

1. Hoe een computer is opgebouwd: de **processor** (CPU), het **geheugen**, en
   hoe ze samenwerken.
2. Dat er verschillende soorten processoren bestaan (x86-64, ARM64, RISC-V) en
   waarom ze een andere "taal" spreken.
3. Hoe een computer opstart, en wat een bootloader precies doet.
4. Hoe je je eigen bootloader schrijft voor **drie** verschillende processoren.
5. Hoe je van een bootloader naar een echte kleine kernel gaat.

## Hoe lees je dit boek?

Lees de hoofdstukken op volgorde. Elk hoofdstuk bouwt voort op het vorige. Typ
de voorbeelden zelf over (niet kopiëren!) - je leert veel meer als je ze met de
hand intikt en fouten maakt. Aan het eind van elk hoofdstuk staat een korte
samenvatting en een paar oefeningen.

## Inhoud

### Deel 1 - Begrijpen

- [Hoofdstuk 0 - Welkom en hoe dit boek werkt](00-welkom.md)
- [Hoofdstuk 1 - Wat is een besturingssysteem?](01-wat-is-een-besturingssysteem.md)
- [Hoofdstuk 2 - Hoe werkt een processor (CPU)?](02-hoe-werkt-een-cpu.md)
- [Hoofdstuk 3 - Architecturen en instructiesets: x86-64, ARM64, RISC-V](03-architecturen-en-instructiesets.md)

### Deel 2 - Klaarmaken

- [Hoofdstuk 4 - Je gereedschap installeren](04-je-gereedschap.md)
- [Hoofdstuk 5 - Hoe een computer opstart](05-hoe-een-computer-opstart.md)

### Deel 3 - Bouwen: je eerste bootloader

- [Hoofdstuk 6 - Bootloader voor RISC-V (de makkelijkste om te beginnen)](06-bootloader-riscv.md)
- [Hoofdstuk 7 - Bootloader voor ARM64](07-bootloader-arm64.md)
- [Hoofdstuk 8 - Bootloader voor x86 (de klassieke)](08-bootloader-x86.md)

### Deel 4 - Verder

- [Hoofdstuk 9 - Van bootloader naar kernel](09-van-bootloader-naar-kernel.md)
- [Hoofdstuk 10 - Wat nu? De weg naar een echt besturingssysteem](10-volgende-stappen.md)

### Deel 5 - Dieper: hoe een besturingssysteem echt werkt

- [Hoofdstuk 11 - Interrupts diepgaand](11-interrupts-diepgaand.md)
- [Hoofdstuk 12 - Virtueel geheugen](12-virtueel-geheugen.md)
- [Hoofdstuk 13 - Systeemaanroepen](13-systeemaanroepen.md)
- [Hoofdstuk 14 - Context wisselen](14-context-wisselen.md)
- [Hoofdstuk 15 - Processen, threads en taken](15-processen-threads-taken.md)
- [Hoofdstuk 16 - Scheduling](16-scheduling.md)
- [Hoofdstuk 17 - CPU-caches](17-cpu-caches.md)
- [Hoofdstuk 18 - Geheugenbeheer](18-geheugenbeheer.md)
- [Hoofdstuk 19 - Drijvende komma en SIMD](19-drijvende-komma-en-simd.md)
- [Hoofdstuk 20 - Atomen en geheugenordening](20-atomen-en-geheugenordening.md)
- [Hoofdstuk 21 - Meerdere processoren (SMP)](21-meerdere-processoren.md)
- [Hoofdstuk 22 - Vergrendelen in de kernel](22-vergrendelen.md)
- [Hoofdstuk 23 - Lock-free strategieen](23-lock-free.md)
- [Hoofdstuk 24 - Drivers en hardware](24-drivers-en-hardware.md)
- [Hoofdstuk 25 - Wachtrijen overal](25-wachtrijen-overal.md)
- [Hoofdstuk 26 - Ringbuffers](26-ringbuffers.md)
- [Hoofdstuk 27 - Async en events](27-async-en-events.md)
- [Hoofdstuk 28 - Zero-copy](28-zero-copy.md)
- [Hoofdstuk 29 - Threads verhuizen tussen CPU's](29-threads-verhuizen.md)
- [Hoofdstuk 30 - Talen en runtimes](30-talen-en-runtimes.md)
- [Hoofdstuk 31 - Tegels en rekenkracht](31-tegels-en-rekenkracht.md)
- [Hoofdstuk 32 - Het wachtrijmodel](32-het-wachtrijmodel.md)

### Extra

- [Woordenlijst - alle moeilijke woorden op een rij](woordenlijst.md)

---

Veel plezier, en niet bang zijn om dingen kapot te maken. Dat hoort erbij.
