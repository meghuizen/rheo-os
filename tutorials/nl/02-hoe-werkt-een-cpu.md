# Hoofdstuk 2 - Hoe werkt een processor (CPU)?

Voordat we een bootloader schrijven, moet je snappen waar die code op draait: de
**processor**. In het Engels heet die de *CPU* (Central Processing Unit). Wij
gebruiken beide woorden door elkaar.

## De processor is een heel snelle domme werker

Een CPU is verrassend "dom". Hij kan alleen maar heel kleine stapjes doen, maar
dan wel miljarden per seconde. Zo'n stapje heet een **instructie**. Voorbeelden:

- "Tel deze twee getallen bij elkaar op."
- "Haal een getal uit het geheugen."
- "Zet een getal in het geheugen."
- "Als dit getal nul is, spring dan naar daar."

Een programma is niets anders dan een hele lange lijst van zulke instructies.
De CPU leest ze een voor een en voert ze uit.

## Registers: de zakjes van de processor

De CPU heeft een paar hele snelle opslagplekjes vlak bij zich. Die heten
**registers**. Zie het als een handvol zakjes waar je één getal in kunt stoppen.
De CPU rekent bijna altijd met de getallen in zijn registers, omdat het geheugen
verder weg is en langzamer.

Voorbeeld (in gewone taal):

1. Stop het getal 5 in register A.
2. Stop het getal 3 in register B.
3. Tel A en B op, zet het resultaat in register A. (A is nu 8.)
4. Zet register A in het geheugen op plek 1000.

Elke soort processor heeft andere namen voor zijn registers. Bij x86 heten ze
bijvoorbeeld `rax`, `rbx`, `rcx`. Bij ARM heten ze `x0`, `x1`, `x2`. Bij RISC-V
heten ze `t0`, `t1`, `a0`. Niet schrikken: het zijn gewoon zakjes met een naam.

Eén register is extra belangrijk: de **program counter** (Nederlands:
programmateller). Daar staat in *welke instructie de CPU nu uitvoert*. Na elke
instructie gaat die teller een stukje omhoog, naar de volgende instructie. Bij
een "spring"-instructie zet de CPU die teller ergens anders neer. Zo ontstaan
lussen en keuzes in een programma.

## Het geheugen: een hele lange rij postbusjes

Het **geheugen** (RAM) is één lange rij van vakjes. Elk vakje heeft een nummer,
en dat nummer heet een **adres**. In elk vakje past een klein getal (meestal
één **byte**, dat is een getal van 0 tot en met 255).

Wil je een groter getal opslaan? Dan gebruik je meerdere vakjes naast elkaar.
Wil je tekst opslaan? Dan zet je in elk vakje één letter (als getal, volgens een
tabel die **ASCII** heet - de letter `A` is bijvoorbeeld het getal 65).

De CPU praat met het geheugen via adressen:

- "Lees het vakje op adres 1000." (dat heet **load**)
- "Schrijf dit getal naar het vakje op adres 1000." (dat heet **store**)

Onthoud dit goed, want onze eerste bootloader doet precies dit: hij schrijft
letters (als getallen) naar een speciaal adres, en dat adres is verbonden met de
seriële poort - waardoor de letters op je scherm verschijnen.

## Bits, bytes en 64-bit

Alles in een computer is uiteindelijk **bits**: nulletjes en eentjes. Acht bits
samen zijn één **byte**.

Je hebt vast "64-bit" gezien. Dat betekent dat de registers van de CPU 64 bits
breed zijn. Er past dus een heel groot getal in één register. Het betekent ook
dat adressen 64 bits kunnen zijn, dus dat de CPU heel veel geheugen kan
aanspreken. De processoren in dit boek zijn alle drie 64-bit.

## Machinecode en assembly

De CPU begrijpt alleen **machinecode**: instructies als kale getallen. Dat is
voor mensen onleesbaar. Daarom schrijven we **assembly** (spreek uit:
"assembelie"). Assembly is machinecode met leesbare namen. Eén regel assembly is
meestal precies één instructie.

Voorbeeld in RISC-V-assembly:

```asm
li t0, 5      # 'load immediate': zet het getal 5 in register t0
li t1, 3      # zet 3 in t1
add t0, t0, t1  # t0 = t0 + t1  (t0 is nu 8)
```

Een programma dat **assembler** heet, vertaalt dit naar machinecode die de CPU
snapt. In dit boek schrijven we onze bootloaders in assembly, omdat we heel
dicht bij de processor werken. Later kunnen we een makkelijkere taal gebruiken.

## Waarom niet gewoon Python of Scratch?

Talen als Python zijn fijn, maar ze hebben een besturingssysteem *nodig* om te
draaien. Wij bouwen juist dat besturingssysteem. Er is dus nog niks. We moeten
in het begin praten in de directe taal van de CPU: assembly. Dat is even wennen,
maar het zijn maar een paar instructies die we nodig hebben.

## Samenvatting

- Een CPU voert heel snel hele kleine **instructies** uit, een voor een.
- **Registers** zijn de snelle zakjes van de CPU waar hij mee rekent.
- De **program counter** wijst naar de instructie die nu aan de beurt is.
- Het **geheugen** is een lange rij vakjes; elk vakje heeft een **adres**.
- **Load** = lezen uit geheugen, **store** = schrijven naar geheugen.
- De CPU snapt alleen **machinecode**; wij schrijven **assembly**, die door de
  **assembler** wordt vertaald.

## Oefening

1. Wat is het verschil tussen een register en een geheugenvakje?
2. Wat doet de program counter?
3. De letter `B` is in ASCII het getal 66. Als je `A` (65) en `B` (66) wilt
   tonen, welke twee getallen schrijf je dan naar de seriële poort?
4. Schrijf in gewone taal (geen echte assembly) de stapjes op om 10 en 20 op te
   tellen en het resultaat in het geheugen op adres 500 te zetten.

Door naar [hoofdstuk 3](03-architecturen-en-instructiesets.md): waarom er
verschillende soorten processoren zijn.
