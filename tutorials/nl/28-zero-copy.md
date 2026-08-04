# Hoofdstuk 28 - Zero-copy: geen onnodige kopie

Stel je voor dat je een brief van een vriend moet doorgeven aan een collega. Je
zou hem kunnen overschrijven op een nieuw blaadje, dat blaadje kopieren op een
derde blaadje, en dat derde blaadje afgeven. Dat is wat een computer vaak doet
met gegevens: kopieren, kopieren, kopieren. Elke kopie kost tijd, en erger nog:
elke kopie vult het snelle geheugen van de processor (de **cache**) met data die
meteen weer vertrekt. In dit hoofdstuk leer je waarom kopieren duur is, en hoe
je het kunt vermijden.

## Waarom is kopieren zo duur?

De processor is snel, het geheugen is traag. Om data te kopieren moet de
processor elk byte lezen van de ene plek en schrijven naar de andere. Dat klinkt
simpel, maar er zijn twee verborgen kosten:

1. **Processortijd**: de CPU moet elke kopie uitvoeren, ook al verandert er niets
   aan de data. Die tijd kan hij niet besteden aan echt nuttig werk.

2. **Cache-vervuiling**: de processor heeft een klein, supersnel stukje geheugen
   vlak bij zich, de **cache**. Daar bewaard hij data die hij net heeft
   aangeraakt, voor het geval hij die zo weer nodig heeft. Maar als je grote
   stukken data kopieert, duw je de nuttige dingen uit de cache. Dat heet
   **cache-vervuiling** en maakt daarna alles langzamer.

Stel het je zo voor: je bureau (de cache) is klein. Als je er een hele stapel
papieren op legt die je alleen maar doorgeeft, lig je eigen werk eronder en
moet je steeds zoeken. Je bureau werkt beter als je die stapel niet aanraakt.

## Het traditionele pad: vier kopieen

Neem het klassieke voorbeeld: een webserver stuurt een bestand naar een bezoeker.
Zonder optimalisatie ziet het er zo uit:

```text
  Schijf
    |
    | 1. Schijf -> kernelbuffer    (DMA, de schijfcontroller kopieert)
    v
  [Kernelbuffer]
    |
    | 2. Kernelbuffer -> gebruikersbuffer  (de processor kopieert)
    v
  [Gebruikersbuffer in je programma]
    |
    | 3. Gebruikersbuffer -> kernelbuffer  (de processor kopieert, terug!)
    v
  [Kernelbuffer voor netwerk]
    |
    | 4. Kernelbuffer -> netwerkkaart      (DMA, de netwerkcontroller kopieert)
    v
  Netwerkkaart -> internet
```

Tel mee: **vier kopieen**. Van die vier doet de processor er twee zelf (stap 2 en
3), en dat zijn precies de stappen die je eigenlijk niet nodig hebt. De data gaat
van de schijf naar je programma en meteen weer terug naar de kernel. Je programma
*kijkt* er misschien niet eens naar.

## Technieken om kopieen te vermijden

### sendfile: sla het programma over

Linux heeft een systeemaanroep genaamd **sendfile**. Die zegt tegen de kernel:
"stuur dit bestand direct naar deze netwerkaansluiting." De data gaat dan van de
schijfbuffer *rechtstreeks* naar de netwerkbuffer, zonder dat je programma eraan
te pas komt. Van vier kopieen naar twee.

```text
  Schijf -> [Kernelbuffer] -> Netwerkkaart
              (geen omweg via het programma)
```

### splice: buizen aan elkaar knopen

**splice** is het algemenere idee: je knoopt twee kanalen (bestanden,
netwerkaansluitingen, buizen) aan elkaar in de kernel. De data stroomt door
zonder dat je programma hem aanraakt. Denk aan een tuinslang die je aan een
andere slang koppelt: het water hoeft niet eerst in een emmer.

### Gedeeld geheugen: samen naar dezelfde plek kijken

In plaats van data te kopieren van A naar B, kun je A en B naar **dezelfde plek
in het geheugen** laten kijken. Dat heet **gedeeld geheugen** (shared memory). Er
wordt niets gekopieerd, omdat er maar een exemplaar is. De truc zit in de
**paginatabellen** van de processor: je vertelt de hardware dat twee processen
dezelfde fysieke pagina mogen zien, elk via hun eigen adres.

### mmap: een bestand als geheugen

Met **mmap** (memory-map) koppel je een bestand aan een stuk geheugen. Als je
programma dat geheugen leest, haalt de processor de data automatisch van de
schijf - via de paginatabellen. Geen expliciete `read`-aanroep, geen kopie van
kernelbuffer naar gebruikersbuffer. De paginatabellen doen het werk.

Dit is een vorm van zero-copy: het besturingssysteem vult de pagina rechtstreeks
met schijfdata als je programma die pagina voor het eerst aanraakt. Dat heet
**demand paging** (pagina's op aanvraag laden).

### Geregistreerde buffers: io_uring

Moderne systemen als **io_uring** laten je buffers *registreren* bij de kernel.
De kernel weet dan precies waar je geheugen ligt, en kan data rechtstreeks daar
neerzetten. Geen extra kopie, geen extra adresvertaling.

## De wisselwerking: snel maar ingewikkeld

Zero-copy is snel, maar het brengt nieuwe vragen mee:

- **Eigenaarschap**: als twee partijen naar dezelfde data kijken, wie mag er dan
  in schrijven? Wie beslist wanneer het geheugen vrij mag? Je hebt regels nodig
  om te voorkomen dat de een de ander's data onder zijn neus vandaan verandert.

- **Beveiliging**: als je een kernelbuffer deelt met een programma, mag dat
  programma dan alles zien wat daar staat? Je moet precies regelen wat zichtbaar
  is en wat niet.

- **Levensduur**: het gedeelde geheugen moet blijven bestaan zolang er iemand
  naar kijkt. Te vroeg vrijgeven betekent een crash; te laat vrijgeven betekent
  een geheugenlek.

Dit is een afweging die je overal in OS-ontwerp tegenkomt: de snelle oplossing
is vaak de ingewikkelde oplossing.

## rheo-os: zero-copy via grants en de queue-pair

In rheo-os is zero-copy een kernidee, geen optimalisatie achteraf. Twee
voorbeelden:

### De queue-pair: gedeelde rijen

De **queue pair** (het rij-paar, in `abi/`) is een stuk gedeeld geheugen tussen
een cel (programma) en de kernel. Kleine berichten gaan **inline** mee in de
rij-invoer. Maar grotere lees- of schrijfacties gaan **by reference**: de invoer
bevat alleen het adres van de data, en de kernel leest of schrijft rechtstreeks
in de geheugen-**grant** van de cel. Geen tussentijdse buffer, geen kopie.

In `librheo/src/io.rs` zie je dit terug:

```text
  Klein bericht (< 64 bytes):
    [rij-invoer met data erin]  ->  kernel leest direct uit de rij

  Groot bericht:
    [rij-invoer met adres]  ->  kernel leest/schrijft in de grant
                                (geen kopie, de cel en kernel kijken
                                 naar dezelfde pagina's)
```

### Compositor: zero-copy beeldoverdracht

In `librheo/src/display.rs` (Phase E) tekent een cel een plaatje in een
geheugen-grant, zegelt die grant (maakt hem onveranderbaar) en deelt hem met een
compositor-cel. De compositor kijkt naar *dezelfde fysieke pagina's* via
`SYS_GRANT_SHARE`, zonder dat er ook maar een byte gekopieerd wordt. Dit is het
zero-copy pad dat ook echte beeldscherm-compositors (zoals Wayland) gebruiken.

```text
  Client-cel                     Compositor-cel
  +-----------------+            +-----------------+
  | tekent plaatje  |            |                 |
  | in grant        |            |                 |
  | zegelt grant    |            |                 |
  | deelt grant ----|--dezelfde->| leest plaatje   |
  |                 |  pagina's  | zonder kopie    |
  +-----------------+            +-----------------+
```

## Samenvatting

- Kopieren kost processortijd en vervuilt de cache met data die meteen weer
  vertrekt.
- Het traditionele pad van schijf naar netwerk bevat **vier kopieen**, waarvan er
  twee onnodig zijn.
- **sendfile** en **splice** laten data in de kernel stromen zonder omweg via het
  programma.
- **Gedeeld geheugen** en **mmap** laten twee partijen naar dezelfde fysieke
  pagina's kijken - nul kopieen.
- **io_uring** registreert buffers zodat de kernel er direct in kan schrijven.
- De wisselwerking: zero-copy is snel maar maakt eigenaarschap, beveiliging en
  levensduur ingewikkelder.
- rheo-os bouwt zero-copy in als kernidee: de queue-pair stuurt grote data
  by-reference en de compositor deelt verzegelde grants zonder kopie.

## Oefeningen

1. Teken het traditionele vier-kopieen-pad op papier. Markeer welke kopieen de
   processor doet en welke de hardware (DMA). Welke twee kun je weglaten?
2. Leg in je eigen woorden uit wat **cache-vervuiling** is. Gebruik de
   bureau-vergelijking.
3. Wat is het verschil tussen **sendfile** en **mmap** als zero-copy-techniek?
   Wanneer zou je welke kiezen?
4. In rheo-os deelt de client-cel een grant met de compositor. Waarom wordt de
   grant eerst **verzegeld** (onveranderbaar gemaakt) voordat hij gedeeld wordt?
   Wat zou er misgaan als dat niet gebeurde?
5. Bekijk `librheo/src/io.rs` in de rheo-os code. Zoek de drempelwaarde
   (`INLINE_MAX`) die bepaalt of data inline of by-reference gaat. Waarom is er
   een drempel in plaats van altijd by-reference?

Door naar [hoofdstuk 29](29-threads-verhuizen.md): threads die verhuizen tussen
processoren.
