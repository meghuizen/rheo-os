# Hoofdstuk 18 - Geheugenbeheer: hoe het OS geheugen uitdeelt

In eerdere hoofdstukken zagen we dat het geheugen een lange rij vakjes is en
dat het OS die vakjes verdeelt over programma's. Maar *hoe* doet het dat
precies? In dit hoofdstuk leer je drie lagen van geheugenbeheer: de
**frame-allocator** in de kernel, **slab-allocatie** voor snelle kleine
stukjes, en **malloc** in je programma. En je leert wat er misgaat als
geheugen versnipperd raakt: **fragmentatie**.

## Het fysieke geheugen: frames

Het geheugen van je computer is opgedeeld in blokken van een vaste grootte.
Zo'n blok heet een **frame** (of **pagina-frame**). De meest gebruikelijke
grootte is **4 KiB** (4096 bytes). Vergelijk het met een groot vel
ruitjespapier dat is opgedeeld in even grote vakken. Elk vak is een frame.

Het OS moet bijhouden welke frames vrij zijn en welke in gebruik. Dat is de
taak van de **frame-allocator**. Er zijn twee veelgebruikte manieren:

### Bitmap-allocator

De simpelste aanpak: een lange rij bits, een per frame. Een `0` betekent
"vrij", een `1` betekent "in gebruik".

```text
Frame:   0  1  2  3  4  5  6  7  8  9 10 11 ...
Bitmap: [1][1][0][0][1][0][0][0][1][1][0][0] ...
              ^  ^     ^  ^  ^        ^  ^
              vrij     vrij vrij      vrij
```

Wil je een frame? Loop door de bitmap tot je een `0` vindt, zet hem op `1`,
en geef het adres van dat frame terug. Wil je een frame teruggeven? Zet zijn
bit terug op `0`.

Voordeel: heel simpel. Nadeel: zoeken kan langzaam zijn als er veel frames
zijn (je loopt soms ver door de bitmap).

In rheo-os werkt de frame-allocator precies zo. De code staat in
`kernel/src/mm/frames.rs`. Het geheugen bestaat uit 131.072 frames van 4 KiB
(512 MiB totaal). De bitmap is een array van 64-bits woorden. Om het zoeken
te versnellen is er een **hint** die onthoudt waar de laatste vrije frame
was gevonden, zodat je niet elke keer bij het begin hoeft te beginnen.

Belangrijk: in rheo-os wordt elk nieuw uitgegeven frame **gewist** (op nul
gezet). Dat is een veiligheidsregel: het vorige programma mag geen geheimen
achterlaten die het volgende programma dan kan lezen.

### Buddy-allocator

Een slimmere aanpak, die veel echte besturingssystemen (waaronder Linux)
gebruiken: het **buddy-systeem**. Het idee: het hele geheugen is een groot
blok. Als je een kleiner blok nodig hebt, splits je het grote blok steeds
in tweeen totdat het de juiste grootte heeft. Elk half is de "buddy" (maatje)
van het andere.

```text
Stap 1: Heel blok (16 frames)
[________________]

Stap 2: Splits in tweeen (8+8)
[________][________]

Stap 3: Splits links nog een keer (4+4+8)
[____][____][________]

Stap 4: Geef het eerste blok van 4 uit
[XXXX][____][________]
 ^
 in gebruik

Teruggeven: plak de twee buddy's weer aan elkaar
[____][____][________]  ->  [________][________]  ->  [________________]
```

Voordeel: als je een blok teruggeeft, kijkt de allocator of het maatje
ook vrij is. Zo ja: ze worden weer samengevoegd. Dat voorkomt
versnippering. Nadeel: je kunt alleen blokken uitdelen waarvan de grootte
een macht van twee is (1, 2, 4, 8, 16... frames). Als je 5 frames nodig
hebt, krijg je er 8 - de rest is verspild.

## Slab-allocatie: voorgevormde bakjes

De frame-allocator deelt geheugen uit in blokken van 4 KiB. Maar de kernel
heeft heel vaak kleine stukjes geheugen nodig: een struct van 48 bytes hier,
een struct van 128 bytes daar. Steeds een heel frame van 4 KiB aanvragen
voor 48 bytes is verspilling.

De oplossing: **slab-allocatie**. Het idee is simpel. Stel dat de kernel
heel vaak een struct van 48 bytes nodig heeft. Dan pakt hij een frame van
4 KiB en verdeelt dat in 85 vakjes van precies 48 bytes (4096 / 48 = 85).
Dat frame vol vakjes heet een **slab**. De kernel pakt een vakje als hij er
een nodig heeft en zet het terug als hij klaar is.

```text
Een slab (1 frame = 4096 bytes), verdeeld in vakjes van 48 bytes:

[vak][vak][vak][vak][vak][vak] ... [vak][vak]
 48B  48B  48B  48B  48B  48B       48B  48B
  X         X                        X
  ^         ^                        ^
  in        in                       in
  gebruik   gebruik                  gebruik
```

Voordeel: supersnel (geen zoekwerk - pak het eerste vrije vakje), geen
interne verspilling, en de objecten liggen netjes naast elkaar in het
geheugen (goed voor de caches uit hoofdstuk 17). Linux gebruikt een
systeem genaamd **SLUB** dat hierop is gebaseerd.

## Malloc in je programma: de gebruikerskant

Als je in C of Rust een stuk geheugen vraagt (`malloc` in C, `Box::new` of
`Vec` in Rust), praat je niet direct met de frame-allocator van de kernel.
Tussen jouw code en de kernel zit een **heap-allocator** in je programma.

Hoe werkt dat?

1. Je programma roept `malloc(100)` aan: "ik wil 100 bytes."
2. De heap-allocator kijkt of hij ergens in zijn voorraad 100 vrije bytes
   heeft.
3. Zo nee: hij vraagt de kernel om meer geheugen, via een syscall
   (`mmap` of `brk`). De kernel geeft dan een of meer frames.
4. De allocator deelt het geheugen op en geeft jou een stukje van 100 bytes.
5. Als je `free()` aanroept, markeert de allocator dat stukje als vrij en
   kan het later opnieuw uitdelen.

```text
  Jouw programma
       |
  malloc(100)
       |
  +----v---------+
  | Heap-allocator|  (in je programma, bijv. glibc's malloc)
  |               |
  | Voorraad:     |
  | [vrij 200B]   |  <- hier past 100B in
  | [bezet 64B]   |
  | [vrij 4000B]  |
  +----+----------+
       |
       | Voorraad op? -> mmap() syscall naar de kernel
       |
  +----v----------+
  | Kernel        |
  | Frame-alloc.  |
  +--------------+
```

In rheo-os staat de heap-allocator voor de runtime in `runtime/src/heap.rs`.
Het is een **vrije-lijst-allocator** (free list): vrije stukken geheugen
worden bijgehouden als een lijst, gesorteerd op adres. Bij het vrijgeven
worden aangrenzende vrije stukken weer samengevoegd, zodat er geen
onbruikbare kleine gaatjes overblijven.

## Fragmentatie: het versnipperingsprobleem

Na een tijdje geheugen uitdelen en terugnemen, kan het geheugen eruitzien als
een gatenkaas: vrije ruimte zit her en der verspreid in kleine stukjes. Dat
heet **fragmentatie**. Er zijn twee soorten:

### Externe fragmentatie

Er is genoeg *totaal* vrij geheugen, maar het zit verspreid in kleine
blokjes die niet naast elkaar liggen. Je kunt geen groot aaneengesloten
stuk meer uitdelen.

```text
Geheugen: [bezet][vrij][bezet][vrij][bezet][vrij][bezet]
                  32B          64B          32B

Totaal vrij: 128 bytes. Maar je kunt geen blok van 100 bytes geven,
want het grootste aaneengesloten vrije stuk is maar 64 bytes.
```

Vergelijk het met een parkeerplaats waar veel losse plekken vrij zijn, maar
nergens twee naast elkaar - een bus past er niet meer op.

### Interne fragmentatie

Je geeft een groter blok dan gevraagd. Als iemand 5 bytes nodig heeft en je
deelt altijd blokken van 8 uit, dan zijn er 3 bytes verspild *binnen* elk
blok. Bij een buddy-allocator die alleen machten van twee kan geven, is dit
onvermijdelijk.

```text
Gevraagd: 5 bytes
Gegeven:  8 bytes
          [XXXXX...]
           ^^^^^---
           nodig  verspild (interne fragmentatie)
```

### Waarom fragmentatie ertoe doet

- Externe fragmentatie kan ervoor zorgen dat het OS "geen geheugen meer
  heeft" terwijl er in totaal nog genoeg vrij is.
- Interne fragmentatie verspilt geheugen bij elke toewijzing.

De frame-allocator (4 KiB blokken) heeft geen externe fragmentatie, want
alle frames zijn even groot en inwisselbaar. Maar als een programma 5 KiB
wil, krijgt het 2 frames (8 KiB) - 3 KiB interne fragmentatie.

De slab-allocator vermindert interne fragmentatie doordat de bakjes precies
de juiste maat hebben. De buddy-allocator vermindert externe fragmentatie
door buddy's samen te voegen. Geen enkel systeem lost allebei perfect op.

## Samenvatting

- Het fysieke geheugen is verdeeld in **frames** van 4 KiB. De
  **frame-allocator** in de kernel houdt bij welke vrij zijn (bitmap of
  buddy-systeem).
- **Slab-allocatie** verdeelt een frame in veel kleine vakjes van dezelfde
  grootte, zodat de kernel snel kleine objecten kan pakken.
- In je programma vraag je geheugen via `malloc`/`free` (of Rust's
  allocator). Die praat met de kernel via `mmap`/`brk` als hij meer
  nodig heeft.
- **Externe fragmentatie**: genoeg vrij geheugen, maar in te kleine
  stukjes verspreid. **Interne fragmentatie**: elk blok is groter dan
  gevraagd.
- rheo-os gebruikt een bitmap-allocator (`kernel/src/mm/frames.rs`) en een
  vrije-lijst-heap (`runtime/src/heap.rs`).

## Oefeningen

1. Je hebt 1 GB geheugen en frames van 4 KiB. Hoeveel frames zijn dat?
   Hoeveel bits heeft de bitmap nodig? En hoeveel bytes is die bitmap?
2. In het buddy-systeem wil je 3 frames. Hoeveel krijg je? Hoeveel is
   verspild? Welk type fragmentatie is dat?
3. Waarom wist rheo-os elk nieuw frame voordat het wordt uitgegeven?
   Wat zou er kunnen misgaan als dat niet gebeurde?
4. Leg in je eigen woorden het verschil uit tussen **externe** en
   **interne** fragmentatie. Gebruik een vergelijking (niet die van de
   parkeerplaats).
5. Bekijk `runtime/src/heap.rs` in rheo-os. Het commentaar zegt dat het
   een "hole list" is. Wat betekent dat? Hoe voorkomt het fragmentatie?

Terug naar de [inhoudsopgave](README.md).
