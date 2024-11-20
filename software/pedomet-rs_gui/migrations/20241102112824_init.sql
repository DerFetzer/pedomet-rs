create table events(
    event_id int not null,
    timestamp_ms int not null,
    boot_id int not null,
    steps int not null
);

create index idx_timestamp_ms on events(timestamp_ms);
create index idx_event_id on events(event_id);
create unique index idx_unique on events(event_id, boot_id);
