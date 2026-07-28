# Hoofdstuk 3 - Architecturen en instructiesets: x86-64, ARM64, RISC-V

In het vorige hoofdstuk zei ik: "elke soort processor heeft andere namen voor
zijn registers." Dat is een groot punt. Er bestaan namelijk verschillende
*soorten* processoren, en ze spreken niet dezelfde taal. In dit hoofdstuk leer
je waarom, en welke drie soorten we in dit boek gebruiken.

## Wat is een instructieset (ISA)?

De lijst met instructies die een processor begrijpt, plus de regels erbij, heet
de **instructieset**. In het Engels: *Instruction Set Architecture*, afgekort
**ISA**. De ISA is als de *taal* van de processor.

Twee processoren met dezelfde ISA begrijpen dezelfde machinecode. Twee
processoren met een andere ISA doen dat niet - net zoals een Nederlander en een
Japanner niet zomaar elkaars taal lezen.

Daarom werkt een programma dat voor de ene ISA gemaakt is, niet zomaar op een
andere. De letters `A` optellen bij `B` is hetzelfde idee, maar de machinecode
ervoor ziet er compleet anders uit.

## De drie ISA's in dit boek

We gebruiken drie ISA's. Waarom drie? Omdat je dan echt *begrijpt* wat overal
hetzelfde is (de ideeën) en wat verschilt (de details). Dat maakt je een veel
betere OS-bouwer.

### 1. x86-64 (ook wel amd64)

- De ISA van de meeste laptops en desktops (Intel- en AMD-processoren).
- Heel **oud** en daardoor ingewikkeld: er zit veel geschiedenis in. Een x86-chip
  van vandaag kan nog steeds programma's uit de jaren 80 draaien.
- Start op een bijzondere manier op (we zien dat in hoofdstuk 8). Dat maakt hem
  het lastigst om mee te beginnen.

### 2. ARM64 (ook wel AArch64)

- De ISA van bijna alle telefoons en tablets, en van nieuwe Apple- en veel
  Windows-laptops, en van veel servers.
- **Netter** ontworpen dan x86: minder rare uitzonderingen.
- Zuinig met stroom, daarom populair in apparaten op een batterij.

### 3. RISC-V (spreek uit: "risk-five")

- Een **open** ISA. Dat betekent: iedereen mag hem gratis gebruiken en er zelfs
  eigen chips mee maken. Bij x86 en ARM moet je betalen en toestemming vragen.
- Heel **eenvoudig** en logisch opgebouwd. Daarom beginnen wij hiermee: het is de
  makkelijkste om te leren.
- Steeds vaker te vinden in nieuwe apparaten en onderzoek.

## Wat is overal hetzelfde?

Hoewel de talen verschillen, zijn de *ideeën* bij alle drie gelijk:

- Er zijn registers.
- Er is een program counter.
- Er is load en store naar het geheugen.
- Er is een kernel-stand en een gebruikers-stand.
- Er is een manier om op te starten en een manier om met hardware te praten.

Als OS-bouwer leer je vooral die ideeën. De precieze instructies zoek je op in
het handboek van de ISA (dat heet de *reference manual*). Niemand kent ze uit
zijn hoofd; iedereen zoekt ze op.

## Grote en kleine verschillen

Een paar dingen die per ISA verschillen en waar je op moet letten:

- **Registernamen.** `rax` (x86) versus `x0` (ARM) versus `a0` (RISC-V).
- **Hoe je een instructie schrijft** (de "spelling", ofwel de *syntax*).
- **Hoe de computer opstart.** Waar begint de CPU met lezen? Op welk adres staat
  onze code? Dit verschilt echt per ISA, en het is precies waarom onze drie
  bootloaders van elkaar verschillen.
- **Endianness.** Dit is een grappig detail: in welke volgorde staan de bytes van
  een groot getal in het geheugen? Onze drie ISA's zetten de "kleinste" byte
  eerst; dat heet **little-endian**. Je hoeft dit nu niet helemaal te snappen,
  maar onthoud het woord.

## Waarom leren we niet gewoon één ISA?

Goede vraag. Veel boeken doen dat wel. Maar dan denk je al snel dat de details
van die ene ISA "de waarheid" zijn, terwijl het maar toevallige keuzes zijn.
Door drie ISA's te zien, leer je het verschil tussen:

- een **idee** dat overal geldt (bijvoorbeeld: "schrijf een byte naar de seriële
  poort om iets te tonen"), en
- een **detail** dat per ISA anders is (bijvoorbeeld: op welk adres die seriële
  poort zit).

Dat is een superkracht voor iemand die besturingssystemen bouwt.

## Emulatie: één QEMU, drie processoren

Het mooie: met QEMU kun je alle drie de processoren nadoen op jouw eigen laptop,
ook al is die zelf misschien een x86 of een ARM. QEMU heeft aparte programma's:

- `qemu-system-x86_64` doet een x86-64 na.
- `qemu-system-aarch64` doet een ARM64 na.
- `qemu-system-riscv64` doet een RISC-V na.

In [hoofdstuk 4](04-je-gereedschap.md) installeren we ze.

## Samenvatting

- De **instructieset (ISA)** is de taal van een processor.
- Verschillende ISA's begrijpen elkaars machinecode niet.
- We gebruiken drie ISA's: **x86-64** (oud, overal), **ARM64** (net, zuinig),
  **RISC-V** (open, eenvoudig - onze startplek).
- De *ideeën* zijn overal hetzelfde; alleen de *details* verschillen.
- Met QEMU doen we alle drie na op één laptop.

## Oefening

1. Wat is een ISA, in je eigen woorden?
2. Waarom draait een x86-programma niet zomaar op een ARM-telefoon?
3. Noem twee dingen die bij alle drie de ISA's hetzelfde zijn, en twee dingen die
   verschillen.
4. Zoek op wat "RISC" betekent (het zit in de naam RISC-V).

Door naar [hoofdstuk 4](04-je-gereedschap.md): we installeren het gereedschap.
