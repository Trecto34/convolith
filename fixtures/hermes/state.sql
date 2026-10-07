
create table schema_version (version integer);
insert into schema_version values (26);
create table sessions (id text primary key, source text, user_id text, model text, model_config text, system_prompt text, parent_session_id text, started_at real, ended_at real, end_reason text, message_count integer, tool_call_count integer, title text, cwd text);
create table messages (id integer primary key autoincrement, session_id text, role text, content text, tool_call_id text, tool_calls text, tool_name text, timestamp real, token_count integer, finish_reason text, reasoning text, reasoning_details text, active integer default 1, compacted integer default 0, _compressed_summary integer default 0);
insert into sessions values('20260830_233734_3ff33e','cli',NULL,'claude-sonnet-4-5',NULL,'You are Hermes.',NULL,1788132454.5,1788132480.0,'done',5,1,'List dir','/home/demo/hermes-proj');
insert into messages(session_id,role,content,tool_call_id,tool_calls,tool_name,timestamp,finish_reason,reasoning) values
 ('20260830_233734_3ff33e','user','list the current directory',NULL,NULL,NULL,1788132455.0,NULL,NULL),
 ('20260830_233734_3ff33e','assistant','',NULL,'[{"id":"call_hermes_1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"ls\"}"}}]',NULL,1788132456.0,'tool_calls','Run ls first.'),
 ('20260830_233734_3ff33e','tool','{"output":"a.txt\nb.txt","exit_code":0,"error":null}','call_hermes_1',NULL,'terminal',1788132457.0,NULL,NULL),
 ('20260830_233734_3ff33e','assistant','Two files: a.txt and b.txt.',NULL,NULL,NULL,1788132458.0,'stop',NULL);
insert into messages(session_id,role,content,timestamp,active,compacted) values ('20260830_233734_3ff33e','user','old turn already compacted',1788132450.0,0,1);
insert into messages(session_id,role,content,timestamp,_compressed_summary) values ('20260830_233734_3ff33e','user','[CONTEXT SUMMARY]: earlier chat about files',1788132451.0,1);
insert into sessions values('20260830_233912_1445cf','telegram',NULL,'gpt-5',NULL,NULL,NULL,1788132552.0,NULL,NULL,1,0,NULL,NULL);
insert into messages(session_id,role,content,timestamp) values ('20260830_233912_1445cf','user','hi from telegram',1788132553.0);
