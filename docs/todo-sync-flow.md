# TODO sync flow, TXT navod

Tento subor je kratky tahak k novemu sync algoritmu. Neobsahuje realne tokeny. V prikladoch pouzivaj iba lokalne cesty, env nazvy a placeholder hodnoty.

## TXT chart

```txt
+-------------------------+
| tjs config setup        |
| uloz config + env nazvy |
+------------+------------+
             |
             v
+-------------------------+
| tjs config show         |
| skontroluj bez tokenov  |
+------------+------------+
             |
             v
+-------------------------+
| tjs status              |
| DB cache + ledger stav  |
+------------+------------+
             |
             v
+-------------------------+
| tjs sync --dry-run      |
| nacitaj Toggl entries   |
+------------+------------+
             |
             v
+-------------------------+
| Najdi issue key         |
| napr. ABC-123           |
+------------+------------+
             |
             v
+-------------------------+
| Issue discovery         |
| hladaj cez vsetky sites |
+------------+------------+
             |
             v
+-------------------------+
| DB cache resolution     |
| issue key -> Jira site  |
+------------+------------+
             |
             v
+-------------------------+
| Site zisti dynamicky    |
| cez Jira discovery      |
| a ulozi do DB cache     |
+------------+------------+
             |
             v
+-------------------------+
| Porovnaj SQLite ledger  |
| Toggl entry <-> worklog |
+------------+------------+
             |
             v
       +-----+------+
       | Zmena?     |
       +-----+------+
             |
      +------+------+
      |             |
      v             v
+-----------+  +----------------+
| dry-run   |  | tjs sync       |
| iba vypis |  | zapis do Jira  |
+-----------+  +-------+--------+
                     |
                     v
              +---------------+
              | uloz mapping  |
              | do SQLite DB  |
              +-------+-------+
                      |
                      v
              +---------------+
              | fallback      |
              | marker pri    |
              | neistom stave |
              +-------+-------+
                      |
                      v
              +---------------+
              | tjs recover   |
              | zrovnaj DB    |
              | s Jira stavom |
              +---------------+
```

## Ako funguje discovery a cache

1. Sync najprv vytiahne issue key z Toggl popisu, napriklad `ABC-123`.
2. Pri prvom stretnuti issue key sa site neberie z config prefixu. Prefixy v configu nie su potrebne.
3. App skusi issue discovery cez vsetky nakonfigurovane Jira sites a zisti, kde issue realne existuje.
4. Vysledok sa ulozi do SQLite DB cache, napriklad `ABC-123 -> blogic`.
5. Dalsie behy pouziju DB cache. Ak je cache neista alebo stav nesedi, fallback marker oznaci zaznam na opatrne spracovanie.
6. SQLite ledger drzi mapovanie `Toggl entry -> Jira worklog`, aby sync vedel robit skip, create, update, delete a recover bez duplicit.

## User commands

```sh
# Prvy setup configu a credentials env names. Tokeny sem nepisat.
tjs config setup

# Ukaz ulozene nastavenia bez realnych secret hodnot.
tjs config show

# Ukaz lokalny stav, DB cache, ledger a posledny sync pohlad.
tjs status

# Bezpecna skuska. Nic nezapisuje do Jira.
tjs sync --dry-run

# Realny sync. Zapise worklogy a ulozi ledger do SQLite.
tjs sync

# Oprava po preruseni behu alebo pri nesulade local DB vs Jira.
tjs recover
```

## Bezpecnostne poznamky

- Nepouzivaj realne tokeny v docs, chate, commitoch ani logoch.
- Config ma obsahovat env nazvy, nie hodnoty tokenov.
- Ked bol token niekde nalepeny omylom, rotuj ho.
- Pre site routing sa nepouziva prefix config. Realny zdroj pravdy je discovery cez Jira sites a nasledna SQLite cache.
