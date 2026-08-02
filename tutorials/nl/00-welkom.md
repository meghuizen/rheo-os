# Hoofdstuk 0 - Welkom en hoe dit boek werkt

## Wat gaan we eigenlijk doen?

Stel je voor dat een computer een leeg gebouw is. Er is stroom, er zijn kamers
(dat is het geheugen) en er is een heel snelle werker (dat is de processor).
Maar er is niemand die zegt wat er moet gebeuren. Een **besturingssysteem** is
de baas van dat gebouw. Het zegt: "Jij programma, jij mag nu even werken. Jij
mag in die kamer. En jij moet wachten."

In dit boek bouw je zelf die baas. We beginnen klein: eerst zorgen we dat de
computer aangaat en dat er tekst op het scherm komt. Dat klinkt simpel, maar het
is een groot moment. Op dat punt draait *jouw* code als allereerste, nog voordat
er iets anders is. Geen Windows, geen macOS, geen Linux. Alleen jij en de
processor.

## Een paar afspraken

- We schrijven "je" en niet "u". Dit boek praat gewoon met jou.
- Nieuwe, moeilijke woorden zetten we **dik gedrukt** en leggen we meteen uit.
  Ze staan ook allemaal in de [woordenlijst](woordenlijst.md).
- Code staat in blokken zo:

  ```text
  dit is code
  ```

- Als je iets moet intikken in een terminal (een venster waar je tekstcommando's
  typt), zie je een `$` ervoor. Die `$` typ je zelf **niet** mee:

  ```console
  $ echo hallo
  hallo
  ```

## Wat is een emulator, en waarom gebruiken we die?

Een echte processor is een stukje silicium in je computer. Je kunt er niet zomaar
"even mee spelen" zonder risico. Daarom gebruiken we een **emulator**: een
programma dat een hele computer *nadoet* in software. Wij gebruiken **QEMU**
(spreek uit als "kjoe-emm-joe"). QEMU doet net alsof het een echte processor is,
met echt geheugen en echte onderdelen.

Voordelen:

- **Veilig.** Als jouw code crasht, sluit je gewoon het venster. Je eigen
  computer merkt er niks van.
- **Snel proberen.** Je hoeft geen echte machine opnieuw op te starten. Je typt
  een commando en je code draait meteen.
- **Drie processoren, één laptop.** Met QEMU kun je doen alsof je een x86-64,
  een ARM64, of een RISC-V computer hebt. Zo leer je alle drie kennen zonder er
  drie te kopen.

## Wat heb je nodig?

- Een computer met Linux, macOS of Windows.
- Zin om te leren en fouten te maken.
- Ongeveer een uur per hoofdstuk. Geen haast.

In [hoofdstuk 4](04-je-gereedschap.md) installeren we alle gereedschappen. Maar
lees eerst de hoofdstukken 1, 2 en 3, zodat je snapt *waarom* we straks doen wat
we doen.

## Belangrijk: het is oké om iets niet te snappen

Een besturingssysteem bouwen is een van de moeilijkste dingen in de
informatica. Profs doen hier jaren over. Dat jij het meteen begint te leren, is
al knap. Als een stuk te snel gaat: lees het rustig nog een keer, of sla het
even over en kom er later op terug. Je hoeft niet alles in één keer te snappen.

## Samenvatting

- Een besturingssysteem is "de baas" die bepaalt welke programma's mogen werken
  en wat ze mogen.
- We bouwen er zelf een, stap voor stap, te beginnen met de allereerste code:
  de bootloader.
- We oefenen veilig met een emulator (QEMU) in plaats van op echte hardware.

## Oefening

1. Schrijf in je eigen woorden op wat een besturingssysteem doet. Gebruik geen
   moeilijke woorden - alsof je het aan een vriend uitlegt.
2. Zoek op internet op wat "QEMU" betekent. (Tip: het is een afkorting.)

Klaar? Ga door naar [hoofdstuk 1](01-wat-is-een-besturingssysteem.md).
