-- Mirrors the table #214 names: the course exercise ends in public.menus, and the question
-- the students could not answer from any tool was "how many rows are in public.menus?".
CREATE TABLE public.menus (
    id_menu      SERIAL PRIMARY KEY,
    paciente_id  INTEGER     NOT NULL,
    fecha        DATE        NOT NULL,
    descripcion  TEXT        NOT NULL,
    calorias     INTEGER
);

INSERT INTO public.menus (paciente_id, fecha, descripcion, calorias) VALUES
    (1, '2026-09-18', 'Puré de patata y merluza al vapor', 620),
    (1, '2026-09-19', 'Arroz hervido con pollo',           710),
    (2, '2026-09-18', 'Crema de calabacín y tortilla',     540),
    (3, '2026-09-18', 'Dieta blanda: caldo y jamón cocido',480),
    (3, '2026-09-19', 'Pescado blanco con verdura',        560);

-- A second table, so a test can prove the tool reads the table it was ASKED for rather than
-- whatever the connection happens to return first.
CREATE TABLE public.pacientes (
    paciente_id  INTEGER PRIMARY KEY,
    nombre       TEXT NOT NULL
);
INSERT INTO public.pacientes VALUES (1,'Ana Gómez'), (2,'Luis Marín'), (3,'Carmen Ruiz');

-- A read-only role: the tool under test must never need write rights.
CREATE ROLE gateway_ro LOGIN PASSWORD 'gateway_ro_pw';
GRANT CONNECT ON DATABASE "Cocina" TO gateway_ro;
GRANT USAGE ON SCHEMA public TO gateway_ro;
GRANT SELECT ON public.menus, public.pacientes TO gateway_ro;
