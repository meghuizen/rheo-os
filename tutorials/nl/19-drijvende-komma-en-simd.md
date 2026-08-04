# Hoofdstuk 19 - Drijvende komma en SIMD: rekenen met kommagetallen en vectoren

Tot nu toe hebben we het vooral gehad over hele getallen: 0, 1, 42, 65535.
Maar een computer moet ook kunnen rekenen met kommagetallen: 3,14 of 0,001
of -273,15. En soms wil je niet een berekening per keer doen, maar vier of
acht tegelijk. In dit hoofdstuk leer je hoe dat werkt, waarom het
ingewikkelder is dan je zou denken, en wat dat betekent voor een kernel.

## Kommagetallen zijn lastig voor een computer

Een computer werkt met bits: nullen en enen. Hele getallen zijn makkelijk:
je telt gewoon in tweetallen (binair). Maar hoe sla je 3,14 op? Of een
heel klein getal als 0,000001? Of een heel groot getal als 602000000000000000000000?

Daarvoor is een slim trucje bedacht: **drijvende komma** (Engels: *floating
point*). Het idee is hetzelfde als wetenschappelijke notatie op school:

- 3,14 schrijf je als 3,14 x 10^0
- 0,000001 schrijf je als 1,0 x 10^-6
- 602 x 10^21

Je slaat drie dingen op: een **teken** (plus of min), een **mantisse**
(de cijfers) en een **exponent** (waar de komma staat). De computer doet
dit in bits in plaats van in cijfers van 0 tot 9. De afspraken hierover
staan in een standaard die **IEEE 754** heet.

Het belangrijkste om te weten: drijvende-kommagetallen zijn **niet exact**.
Net zoals je 1/3 niet precies kunt opschrijven als kommagetal (0,3333...),
kan de computer veel getallen niet precies opslaan. Dat levert soms kleine
afrondingsfouten op. Dat is normaal en zelfs gewenst, zolang je het weet.

## De FPU: een rekenmachine in de rekenmachine

De eerste processoren konden alleen met hele getallen werken. Als je wilde
rekenen met kommagetallen, moest de software dat zelf doen: het programma
deed alsof de processor een komma had, maar gebruikte alleen gewone
optelinstructies en verschuivingen. Dat is heel langzaam.

Later kregen processoren een apart stukje hardware voor kommaberekeningen:
de **FPU** (Floating Point Unit). Zie het als een extra rekenmachine die
in de processor zit, speciaal gebouwd voor kommagetallen. Die heeft zijn
eigen **registers** (zijn eigen zakjes) waar kommagetallen in passen.

## Soft float versus hard float

Er zijn dus twee manieren om met kommagetallen te rekenen:

- **Soft float** (zachte drijvende komma): de compiler vertaalt elke
  kommaberekening naar een reeks gewone instructies. Geen speciale hardware
  nodig, maar het is **langzaam** - een simpele optelling kost tientallen
  instructies in plaats van een.

- **Hard float** (harde drijvende komma): de processor heeft echte
  FPU-instructies en FPU-registers. Een optelling is **een instructie**.
  Veel sneller, maar die extra registers moeten wel worden bewaard als je
  wisselt tussen programma's.

Stel je voor dat je een ballon hebt met "soft" op de ene kant en "hard" op
de andere. Soft is flexibel (werkt overal), hard is stevig (gaat snel).

## Waarom de kernel vaak soft float gebruikt

Hier wordt het interessant voor ons als OS-bouwers. De kernel schakelt
heel vaak heen en weer tussen programma's (dat heet een **context switch**,
zie hoofdstuk 4). Bij elke wissel moet de kernel alle registers van het
oude programma bewaren en de registers van het nieuwe programma laden.

Als de kernel zelf de FPU-registers gebruikt, moet hij die *ook* bewaren
bij elke **trap** (elk moment dat de kernel wordt aangeroepen). Dat kost
extra tijd, zelfs als de trap niets met kommagetallen te maken heeft.

Daarom kiezen veel kernels ervoor om zelf **geen** FPU-instructies te
gebruiken. De kernel rekent alleen met hele getallen (soft float of gewoon
geen kommagetallen). Alleen de gebruikersprogramma's (de "cellen" in
rheo-os) mogen de FPU gebruiken.

In rheo-os is dit precies zo:
- De **kernel** is soft-float: hij raakt de FPU-registers nooit aan.
- De **cellen** (gebruikersprogramma's) zijn hard-float: ze mogen de FPU
  volop gebruiken.
- Bij een wissel tussen cellen bewaart de kernel de FPU-registers van de
  oude cel en laadt die van de nieuwe. Die code zit per ISA in
  `kernel/src/arch/*/mod.rs`.

## SIMD: meer tegelijk doen

Nu wordt het nog leuker. Stel je voor dat je vier getallen hebt en je wilt
ze allemaal verdubbelen. Normaal doe je dat een voor een:

```text
Scalair (gewoon):

  Stap 1:   [A] --x2--> [A*2]
  Stap 2:   [B] --x2--> [B*2]
  Stap 3:   [C] --x2--> [C*2]
  Stap 4:   [D] --x2--> [D*2]

  Vier instructies nodig.
```

Maar met **SIMD** (Single Instruction, Multiple Data) doe je het in een
keer:

```text
SIMD (vector):

  Stap 1:   [A | B | C | D] --x2--> [A*2 | B*2 | C*2 | D*2]

  Een instructie voor vier getallen tegelijk!
```

SIMD werkt met brede registers die meerdere getallen naast elkaar bevatten.
Zie het als een breed bakje waarin vier gewone getallen passen. Een
SIMD-instructie doet dezelfde bewerking op alle getallen in het bakje
tegelijk.

Elke ISA heeft zijn eigen naam voor SIMD:

- **x86-64**: SSE (128 bits breed, 4 floats tegelijk), AVX (256 bits, 8
  floats), AVX-512 (512 bits, 16 floats)
- **ARM64**: NEON (128 bits, 4 floats)
- **RISC-V**: de F- en D-extensies (voor losse floats/doubles) en de
  V-extensie (voor echte vectoren)

SIMD is enorm belangrijk voor rekenwerk: beeldbewerking, geluid, kunstmatige
intelligentie (matrix-vermenigvuldigingen), en alles waarbij je veel
dezelfde bewerking op veel getallen doet.

## FMA: als een plus b keer c anders uitkomt

Een bijzonder geval is **FMA** (Fused Multiply-Add). Dat is de berekening
`a * b + c` in **een stap** in plaats van twee.

Waarom maakt dat uit? Omdat de computer tussentijds afrondt. Als je eerst
`a * b` uitrekent (met afronding) en daarna `+ c` (weer afronding), krijg
je *twee* afrondingen. Met FMA is er maar *een* afronding, aan het eind.

Dat klinkt als een klein verschil, maar het heeft twee gevolgen:

1. **FMA is preciezer.** Minder afrondingsfouten.
2. **FMA geeft een ander resultaat** dan twee losse stappen. Dat betekent
   dat een programma gecompileerd met FMA een iets ander antwoord kan geven
   dan hetzelfde programma zonder FMA. Dat maakt binaries soms
   **onverenigbaar**: het ene programma verwacht het FMA-resultaat, het
   andere niet.

In rheo-os wordt dit opgelost door bewuste keuzes per onderdeel. De
tile-kernels in `librheo/src/tile/` kiezen expliciet of ze FMA gebruiken,
zodat het resultaat voorspelbaar is.

## Samenvatting

- **Drijvende komma** (floating point) is hoe een computer kommagetallen
  opslaat, met een teken, mantisse en exponent (IEEE 754). Het is niet
  exact.
- **Soft float**: kommaberekeningen in gewone instructies (langzaam, werkt
  overal). **Hard float**: echte FPU-hardware (snel, registers moeten
  bewaard worden).
- De kernel van rheo-os is soft-float om te voorkomen dat FPU-registers
  bij elke trap bewaard moeten worden. Cellen zijn hard-float.
- **SIMD** doet dezelfde bewerking op meerdere getallen tegelijk (SSE/AVX
  op x86, NEON op ARM, V op RISC-V).
- **FMA** (a*b+c in een stap) is preciezer maar geeft een ander resultaat
  dan twee losse stappen.

## Oefeningen

1. Leg in je eigen woorden uit waarom 0,1 + 0,2 op een computer niet
   precies 0,3 is.
2. Waarom gebruikt de kernel van rheo-os geen FPU-instructies? Wat zou er
   gebeuren als hij dat wel deed?
3. Je hebt acht getallen en wilt ze allemaal met 3 vermenigvuldigen. Hoeveel
   instructies kost dat scalair? En hoeveel met een 256-bits SIMD-register
   dat vier floats tegelijk kan verwerken?
4. Wat is het verschil tussen soft float en hard float? Wanneer zou je soft
   float kiezen?
5. Waarom kan een programma gecompileerd met FMA een ander antwoord geven
   dan hetzelfde programma zonder FMA?

Door naar [hoofdstuk 20](20-atomen-en-geheugenordening.md): hoe meerdere
processoren veilig dezelfde data kunnen aanraken.
