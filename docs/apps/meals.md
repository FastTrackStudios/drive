# Meals: cooking from building blocks

A Drive app for planning, prepping and cooking with **components**
(building blocks you make once and combine many ways) instead of fixed
recipes. Written 2026-11-20.

## The idea, from the video

Ethan Chlebowski, *How to Eat Chicken Breast Every Day (without going
crazy)*, 18 Nov 2026 (https://youtu.be/1m9ReeKwJ4I), teaches three levels,
each a framework that multiplies combinations:

1. **Technique.** One way to cook a base well: flatten the breast to an
   even thickness, salt at 1–1.5% of its weight (2 g for 200 g), spice
   generously (check blends for salt), sear in a hot pan (~425 °F surface)
   that stays hot. The cooked base then goes into **form factors**: a wrap,
   a sandwich, a salad, a rice bowl. Four spice blends × four form factors
   = 16 meals from one technique.
2. **Deferred flavour.** Batch-cook the base plain (whole, strips or
   cubes; 1% salt), store it unflavoured, and flavour it when eating, by
   one of three **methods**: bloom spices in oil in the pan and toss the
   cold base through; toss the warm base with a dressing or vinaigrette
   (chimichurri); glaze it with a sauce in the pan (teriyaki, made ahead
   by weight and kept in a squeeze bottle). 3 methods × 4 variations × 4
   form factors = 48 meals from one prep.
3. **The plate.** Four slots: a **protein** (seared, roasted, grilled, or
   breaded and fried), a **starch** (mash, rice, bread, tortillas, orzo),
   a **vegetable** (a salad, roast vegetables, a slaw, pickles), and a
   **sauce** (a pan sauce, tomato, curry, a mayo-based dressing). Four of
   each = 256 plates. Classic dishes (chicken parm, piccata, katsu curry)
   are instances of it. Components (a Caesar dressing, a sprout slaw, a
   parmesan orzo) are prepped ahead and reused across meals.

Throughout, amounts are by weight, and each meal gets a calorie, protein
and cost breakdown.

**What the app takes from it:** components are the unit, not recipes;
flavour is often a separate component applied late; meals are slots
filled from what is prepped; the number of possible meals is the point,
and the app should show it.

## The landscape (2026)

- **Component-first apps are rare and small:** PrepFlow (iOS: "cook a few
  useful bases once and turn them into different dinners", tracks
  prepared bases); Meal Prep for Batch Cooking UK (batch-cook
  "Foundations"); Mealprapp (a German "Meal Prep Baukasten", tied to a
  course). None models methods, slots and the fridge together.
- **Recipe-first planners:** Paprika, Plan to Eat (manual, shared), Mealime,
  Samsung Food (free, grocery retailers), Prepear; **AI planners:** Ollie
  (iOS, families), Eat This Much (macros); **macro-first:** MealPrepPro.
  **Build-your-own bowl** ordering (The Meal Preps, Qdoba, Luna Grill)
  shows the slot-and-macros UI people already know.
- **Self-hosted, open source:** Mealie, Tandoor (recipe managers). From
  memory; not checked for this note.
- **Recipe text:** Cooklang, a plain-text recipe markup with a Rust parser
  (`cooklang` crate, 0.18.x: scaling, unit conversion, aisles).

## The model (Drive document types, namespace `meals.`)

| Type | What it is |
|---|---|
| `Ingredient` | Something bought: nutrition per 100 g, cost per unit, aisle, how it keeps. A shared catalog per household, seeded from public data (USDA FoodData Central). |
| `Technique` | A way of cooking a kind of base, with parameters: salt % by weight, pan temperature, time by cut (whole, strips, cubes), and steps. |
| `Component` | A building block you make and store: plain salted chicken cubes, teriyaki sauce, chimichurri, Caesar dressing, sprout slaw, parmesan orzo. Has **roles** (protein, starch, vegetable, sauce, flavour, topping), a recipe (Cooklang text, so it scales and converts), a yield in grams, how it is stored and how long it keeps. Nutrition and cost per gram are computed from its ingredients. |
| `Flavour` | A component applied late, with a **method**: bloom in pan, toss with dressing, glaze with sauce. Usable on any base whose role allows it. |
| `Template` | A form factor or framework with **slots**: the plate (protein, starch, vegetable, sauce), a rice bowl (base, protein, flavour, vegetable, topping), a wrap, a sandwich, a salad. Each slot accepts roles and has a default amount. |
| `Meal` | A template with its slots filled and amounts set. Its calories, protein and cost are computed. Can be saved, rated, repeated. |
| `Plan` | Meals placed on days and times for a household. |
| `PrepSession` | A batch: which components to make and how much, worked backwards from the plan; an ordered, timed checklist. |
| `Fridge` | What is prepped and on hand: each component's amount left and when it was made (so when it expires), and raw stock. |
| `ShoppingList` | What the prep needs, minus what is on hand, by aisle. |

## Steps and workflows

Every component, flavour and meal is made by a **workflow**: steps with
inputs, equipment, parameters, timing and a way to know each step is
done. Workflows compose, so a prep session or a dinner is one workflow
built from the components' own.

### A step

| Field | Example (level 1, the sear) |
|---|---|
| Action | sear |
| Inputs, with **quantity rules** | chicken breast (the parameter `w`, grams); salt **1–1.5% of `w`**; black pepper **generous**; cooking oil 5 g |
| Equipment | cast-iron pan |
| Parameters | pan surface ~425 °F; the pan must stay hot (a heavy pan) |
| Time | ~5 min, **active**; depends on thickness and cut |
| **Done when** | internal 165 °F; browned both sides |
| **Why** | an even thickness cooks evenly: the centre reaches 165 before the outside dries |
| Cautions | a spice blend with salt in it: less salt |
| Media | a photo or a short clip of the step |

**Quantity rules** are what recipes lack: absolute (60 g soy sauce), a
ratio of a parameter (salt 1% of the chicken), a range (1–1.5%), or a
judgement (generous, to taste, a spoonful or two). Ratios recompute as
the batch scales.

**Time** is split into **active** (hands on) and **passive** (simmering,
resting, chilling), so a schedule can overlap them.

### A workflow

- **Parameters** for the whole workflow: the chicken's weight, how many
  portions.
- **Variants** that change steps: the cut (whole, strips, cubes) changes
  the sear time; the cooking method (pan-sear, roast, grill, bread and
  fry) swaps whole steps.
- **Sub-workflows:** a breading station (flour, egg and milk, panko;
  press the crumbs in) is its own workflow, used inside "breaded cutlet";
  level 1's sear is used inside level 2's batch cook.
- **Steps form a graph, not a list:** each step says what it needs
  (the dressing before the slaw; the orzo can run while the cutlets fry).
- **Output:** the component it makes, its yield in grams, how it is
  stored (a squeeze bottle, a sealed tub) and how long it keeps.

### The video as workflows

| Workflow | Steps | Output |
|---|---|---|
| Plain salted chicken (level 2) | flatten (or cut into strips or cubes) → salt 1% of `w` → **sear** (time by cut) → cool → store | plain chicken, ~4 days, fridge |
| Bloom in pan (flavour method) | 5 g oil in a hot pan → spice blend, bloom 20–30 s → cold chicken in, toss → heat through | smoky spicy chicken, to eat now |
| Toss with dressing (method) | warm the chicken (microwave) → toss with a dressing | chimichurri chicken, now |
| Glaze (method) | sauce into a pan → chicken in → heat until glossy | teriyaki chicken, now |
| Teriyaki sauce | pot on the scale: soy 60 g, mirin 60 g, sake 60 g, sugar 25 g, ginger ~1 in grated, 1 clove garlic → simmer ~5 min (**passive**) → cornstarch slurry, thicken → cool → squeeze bottle | teriyaki sauce, fridge |
| Chimichurri | parsley, dried oregano, garlic, chilli flakes, olive oil, red wine vinegar → mix | chimichurri, fridge |
| Breaded cutlet (level 3) | flatten into two cutlets → salt → **breading station** (sub-workflow) → shallow-fry in peanut oil until golden, to temperature | cutlets, now |
| Sprout slaw | finely chop Brussels sprouts → half a shallot, thinly sliced → toss with a spoonful or two of Caesar dressing | slaw, fridge |
| Parmesan orzo | boil in salted water 6–8 min (**passive**) → drain → butter ~2 tbsp, parmesan a generous handful, black pepper | orzo, now or fridge |
| The plate (assembly) | slaw on the left → orzo on the right → cutlet in the centre → drizzle dressing over | the meal: ~840 kcal, ~60 g protein |

The flavour **methods** are workflows with a slot: "bloom in pan" takes
any spice blend, "glaze" any sauce. That is how 3 methods × 4 flavours
become 12 workflows without writing 12.

### Prep sessions and cook mode

- **Scheduling:** a prep session merges the workflows of everything the
  plan needs into one timeline: it orders steps by what they need,
  overlaps passive time (the teriyaki simmers while the chicken is
  flattened), respects equipment (one cast-iron pan, one oven) and
  people, and says when to start so everything is ready together.
- **Cook mode:** one step at a time, big, with its quantities already
  scaled, its timers running, and its "done when"; a step's timer and its
  checkpoint ("165 °F?") gate the next.
- **Together, live:** two people in the kitchen split the timeline;
  each sees the other's steps tick off (Drive's realtime sync and
  presence).
- **Learning:** actual times and notes recorded per run, so the next
  estimate is better ("your cubes take 6 min, not 5").

### Text, in and out

Workflows are structured documents (so they sync, merge and schedule).
**Cooklang** is their text form, for import, export and quick entry: it
already has ingredients, cookware (`#`), timers (`~{5%minutes}`),
sections, and references to other recipes with a multiplier
(`@./sauces/teriyaki{2}`). What Cooklang lacks (ratios of a parameter,
"done when", variants, active and passive time) lives in the structured
model, written into Cooklang's metadata when exported.

## What the app does

1. **Build blocks.** Add components (from a recipe, a technique applied
   to an ingredient, or a flavour), each with roles and how it keeps.
2. **Plan with slots.** Pick a template for a meal and fill its slots;
   the plan rolls up the week's calories, protein and cost.
3. **Prep backwards.** The plan becomes a prep session: what to cook,
   how much, in what order, and what to buy.
4. **Cook from the fridge.** "What can I make tonight": every template
   whose slots can be filled from what is prepped, ranked by what expires
   first and what you have not eaten lately; the count of possible meals
   shown, as in the video ("48 meals from this prep").
5. **Prep and cook by workflow.** The prep session's timeline, then cook
   mode, step by step, alone or together.
6. **Eat and log.** Taking a meal draws its amounts from the fridge and
   logs its nutrition.

## On Drive

- **Spine:** a prep session is an **Event** in the calendar; the
  shopping list's items go to the **inbox** as Tasks (or stay a list,
  linked); meals can belong to a **Project** (a cut, a party).
- **A household is an org:** shared plan, fridge and shopping list,
  edited live (two people ticking off prep steps).
- **Links:** a plan or a component shared with a guest, nothing
  installed.
- **Cards:** "Tonight: teriyaki chicken bowl, 657 kcal, 43 g protein";
  "Prep Sunday 2 pm: 4 components".
- **Media:** photos and videos of components and meals as blobs.

## First prototype (M9 of the plan)

The video's ten workflows above as seed data; steps with quantity rules,
active and passive time and "done when"; variants and sub-workflows;
components with roles and keeping times; three templates (plate, rice
bowl, wrap); planning a week; the prep session's timeline and shopping
list worked out from it; cook mode with timers; "what can I make
tonight" from the fridge; the prep session's Event in the Drive app.
Nutrition from a small seeded catalog; Cooklang import and export.
