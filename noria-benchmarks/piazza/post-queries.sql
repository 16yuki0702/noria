--QUERY posts: select * from Post where p_cid = ?;
--QUERY post_count: select p_cid, count(p_id) from Post where p_cid = ? group by p_cid;
QUERY post_count: select p_author, count(p_id) from Post where p_author = ? group by p_author;
