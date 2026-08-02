# Hoofdstuk 5 - Hoe een computer opstart

Je drukt op de aan-knop. Een paar seconden later zie je je bureaublad. Maar wat
gebeurt er in die seconden? Dat is precies wat wij gaan overnemen. In dit
hoofdstuk leer je het opstartproces, en waar onze bootloader in past.

## Het probleem bij het aanzetten

Als de computer net aangaat, is het geheugen leeg. Er staat nog geen enkel
programma. Maar de processor wil meteen instructies uitvoeren. Waar haalt hij die
vandaan?

Het antwoord: de processor is zo gebouwd dat hij bij het aanzetten *altijd op een
vaste plek* begint met lezen. Die plek is voor elke ISA anders, maar het idee is
overal hetzelfde: "bij het opstarten, begin te lezen op adres X."

Op die vaste plek staat de allereerste code. Vroeger stond die in een chip op het
moederbord. Die eerste code heet de **firmware**.

## Firmware: de code die er al is

**Firmware** is software die vast in een chip zit, klaar zodra de stroom aangaat.
Bekende namen:

- **BIOS** - de oude firmware van pc's.
- **UEFI** - de moderne opvolger van de BIOS.
- Op ARM- en RISC-V-bordjes heet het vaak gewoon "de bootrom" of een programma
  als **U-Boot** of **OpenSBI**.

De firmware doet een paar dingen:

1. Ze test de belangrijkste onderdelen (geheugen, enzovoort).
2. Ze zoekt een plek waar een besturingssysteem staat (een schijf, een USB-stick,
   het netwerk).
3. Ze laadt daar het eerste stukje van in het geheugen en springt ernaartoe.

Dat eerste stukje dat de firmware laadt en start, is de **bootloader**.

## Wat is een bootloader precies?

Een **bootloader** is een klein programma met één hoofdtaak: **de kernel starten**.

De firmware is namelijk simpel. Ze weet niet hoe jouw besturingssysteem in elkaar
zit. Ze weet alleen: "laad dit eerste blokje en spring erin." De bootloader is
dat eerste blokje. De bootloader weet wél waar de kernel staat, laadt die in het
geheugen, zet een paar dingen goed, en springt dan naar de kernel.

De keten ziet er zo uit:

```text
stroom aan
   |
   v
firmware (BIOS/UEFI/OpenSBI)   <- zit al in de chip
   |
   v
bootloader                     <- JOUW eerste code
   |
   v
kernel                         <- de rest van je OS
```

In dit boek maken we het onszelf in het begin makkelijk: we laten QEMU onze code
*direct* laden, zodat we niet meteen met een echte schijf en firmware hoeven te
stoeien. Onze code is dan tegelijk een heel klein beetje bootloader en een heel
klein beetje kernel. Later splitsen we dat netjes.

## Waarom onze eerste bootloader alleen tekst toont

Een bootloader die meteen een hele kernel start, is te veel voor stap één. Daarom
doet onze allereerste bootloader iets veel simpelers, maar heel bevredigends: hij
laat tekst zien. Bijvoorbeeld "Hallo van RISC-V!".

Waarom is dat al een overwinning? Omdat het betekent dat:

- de firmware onze code heeft geladen;
- de processor onze instructies uitvoert;
- wij een echt stukje hardware (de seriële poort) hebben aangestuurd.

Alle grote dingen die daarna komen - geheugen verdelen, programma's starten -
bouwen voort op dit ene moment.

## De seriële poort: onze eerste "hardware"

Hoe laten we tekst zien zonder besturingssysteem? We gebruiken de **seriële
poort** (Engels: *serial port* of *UART*). Dat is een heel oud en heel simpel
stukje hardware dat één voor één tekens verstuurt.

Het mooie: je stuurt een teken door één byte te **schrijven naar een vast adres**
in het geheugen. Weet je nog, uit hoofdstuk 2? Store naar een adres. Dat adres is
niet echt geheugen, maar een "deurtje" naar de seriële poort. Dit heet
**memory-mapped I/O**: hardware aansturen door naar speciale adressen te schrijven.

- Op RISC-V (QEMU virt) zit dat deurtje op adres `0x10000000`.
- Op ARM64 (QEMU virt) op adres `0x09000000`.
- Op x86 werkt het net iets anders (via de firmware, of via een "I/O-poort"); dat
  zien we in hoofdstuk 8.

QEMU verbindt die seriële poort met jouw terminal. Dus alles wat onze bootloader
naar dat adres schrijft, verschijnt in het venster waar je QEMU startte. Perfect
om mee te beginnen.

## Even over adressen zoals 0x10000000

Dat rare `0x` betekent: dit getal staat in het **hexadecimale** stelsel (kortweg
"hex"). Wij tellen met tien cijfers (0-9). Hex telt met zestien: 0-9 en dan
a, b, c, d, e, f. Programmeurs gebruiken hex omdat het mooi past bij hoe
computers met bits werken. Je hoeft nu alleen te weten: `0x10000000` is gewoon een
(groot) adres. Je hoeft het niet uit je hoofd om te rekenen.

## Samenvatting

- Bij het aanzetten begint de processor op een vaste plek te lezen.
- Daar staat de **firmware** (BIOS/UEFI/OpenSBI): die test de boel en laadt de
  **bootloader**.
- De **bootloader** is jouw eerste code; zijn taak is de **kernel** starten.
- Onze eerste bootloader toont alleen tekst via de **seriële poort**, door een
  byte te schrijven naar een vast adres (**memory-mapped I/O**).
- Adressen als `0x10000000` staan in **hex**; het is gewoon een getal.

## Oefening

1. Zet de opstartketen op volgorde: kernel, firmware, bootloader, stroom aan.
2. Wat is de hoofdtaak van een bootloader?
3. Wat betekent "memory-mapped I/O"?
4. Waarom is "tekst tonen" al een grote overwinning voor onze eerste bootloader?

Nu begint het echte werk. Door naar [hoofdstuk 6](06-bootloader-riscv.md): je
eerste bootloader, voor RISC-V.
